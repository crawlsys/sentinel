//! BA1+3 Phase 4c — Enforcement-mode config loader.
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §3. Loads operator
//! `ValidationMode` choices for `provenance_validate` and
//! `requirements_traceability_gate` from a shipped TOML baseline
//! merged with an optional operator override at
//! `~/.claude/sentinel/config/ba-enforcement.toml`.
//!
//! ## Shipped defaults
//!
//! Both hooks default to `ObserveOnly` — the safe rollout posture
//! per spec §3. The hook still runs all four checks per citation and
//! logs would-be blocks via `tracing::warn`, but never returns
//! `deny()`. Operators flip modes to `DefaultBlocking` once
//! telemetry shows the connector + matrix layers reliably emit
//! audit + matrix data; flip to `StrictBlocking` for
//! catastrophic-class output tools.
//!
//! ## TOML shape
//!
//! ```toml
//! [provenance_validate]
//! mode = "ObserveOnly"  # ObserveOnly | DefaultBlocking | StrictBlocking
//!
//! [requirements_traceability_gate]
//! mode = "ObserveOnly"
//! ```
//!
//! Both keys are optional; missing keys fall back to `ObserveOnly`.
//! Unknown mode strings surface as a load error — operators see
//! the typo at startup rather than discovering silent
//! mis-enforcement at production time.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use sentinel_application::hooks::{provenance_validate, requirements_traceability_gate};
use serde::Deserialize;

/// Shipped baseline. Both hooks default to `ObserveOnly`.
pub const SHIPPED_BA_ENFORCEMENT_DEFAULTS: &str = include_str!(
    "../../../config/ba-enforcement-defaults.toml"
);

/// Operator-facing mode wrapper for both BA1+3 hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaEnforcementConfig {
    pub provenance_validate_mode: provenance_validate::ValidationMode,
    pub requirements_traceability_mode: requirements_traceability_gate::ValidationMode,
}

impl BaEnforcementConfig {
    /// Safe default: both hooks in `ObserveOnly` (no blocking;
    /// telemetry only).
    #[must_use]
    pub const fn observe_only() -> Self {
        Self {
            provenance_validate_mode: provenance_validate::ValidationMode::ObserveOnly,
            requirements_traceability_mode:
                requirements_traceability_gate::ValidationMode::ObserveOnly,
        }
    }

    /// Parse from a TOML string. Missing keys fall back to
    /// `ObserveOnly`; unknown mode strings surface as `Err`.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let toml_doc: BaEnforcementToml =
            toml::from_str(s).context("failed to parse ba-enforcement TOML")?;
        let provenance_validate_mode = toml_doc
            .provenance_validate
            .as_ref()
            .map_or(Ok(provenance_validate::ValidationMode::ObserveOnly), |s| {
                parse_provenance_mode(&s.mode)
            })?;
        let requirements_traceability_mode = toml_doc
            .requirements_traceability_gate
            .as_ref()
            .map_or(
                Ok(requirements_traceability_gate::ValidationMode::ObserveOnly),
                |s| parse_requirements_mode(&s.mode),
            )?;
        Ok(Self {
            provenance_validate_mode,
            requirements_traceability_mode,
        })
    }

    /// Load the shipped defaults (compile-time embedded TOML).
    pub fn shipped() -> Result<Self> {
        Self::from_toml_str(SHIPPED_BA_ENFORCEMENT_DEFAULTS)
            .context("failed to parse shipped ba-enforcement-defaults.toml")
    }

    /// Load shipped defaults + apply operator overrides from `path`
    /// if the file exists. Missing path → shipped only.
    pub fn with_shipped_and_overrides(overrides_path: Option<&Path>) -> Result<Self> {
        let mut config = Self::shipped()?;
        if let Some(path) = overrides_path {
            if !path.exists() {
                tracing::debug!(
                    "no operator ba-enforcement.toml at {} — using shipped defaults",
                    path.display()
                );
                return Ok(config);
            }
            let content = std::fs::read_to_string(path).with_context(|| {
                format!("failed to read ba-enforcement.toml at {}", path.display())
            })?;
            let overrides = Self::from_toml_str(&content).with_context(|| {
                format!(
                    "failed to parse operator ba-enforcement.toml at {}",
                    path.display()
                )
            })?;
            // Override: each key set in the operator file wins; the
            // shipped value stays otherwise. Since both modes are
            // always present after `from_toml_str` (defaulting to
            // ObserveOnly), this collapses to: operator file
            // completely replaces shipped. Future enhancement: parse
            // overrides separately so missing-key passes through.
            config = overrides;
        }
        Ok(config)
    }
}

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct BaEnforcementToml {
    #[serde(default)]
    provenance_validate: Option<ModeSection>,
    #[serde(default)]
    requirements_traceability_gate: Option<ModeSection>,
}

#[derive(Debug, Deserialize)]
struct ModeSection {
    mode: String,
}

fn parse_provenance_mode(s: &str) -> Result<provenance_validate::ValidationMode> {
    match s {
        "ObserveOnly" => Ok(provenance_validate::ValidationMode::ObserveOnly),
        "DefaultBlocking" => Ok(provenance_validate::ValidationMode::DefaultBlocking),
        "StrictBlocking" => Ok(provenance_validate::ValidationMode::StrictBlocking),
        other => Err(anyhow!(
            "unknown provenance_validate.mode {other:?}; expected ObserveOnly | DefaultBlocking | StrictBlocking"
        )),
    }
}

fn parse_requirements_mode(s: &str) -> Result<requirements_traceability_gate::ValidationMode> {
    match s {
        "ObserveOnly" => Ok(requirements_traceability_gate::ValidationMode::ObserveOnly),
        "DefaultBlocking" => Ok(requirements_traceability_gate::ValidationMode::DefaultBlocking),
        "StrictBlocking" => Ok(requirements_traceability_gate::ValidationMode::StrictBlocking),
        other => Err(anyhow!(
            "unknown requirements_traceability_gate.mode {other:?}; expected ObserveOnly | DefaultBlocking | StrictBlocking"
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_parse_cleanly() {
        let config = BaEnforcementConfig::shipped().unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::ObserveOnly
        );
        assert_eq!(
            config.requirements_traceability_mode,
            requirements_traceability_gate::ValidationMode::ObserveOnly
        );
    }

    #[test]
    fn observe_only_constant_matches_shipped() {
        let observe = BaEnforcementConfig::observe_only();
        let shipped = BaEnforcementConfig::shipped().unwrap();
        assert_eq!(observe, shipped);
    }

    #[test]
    fn parses_default_blocking_explicit() {
        let toml = r#"
[provenance_validate]
mode = "DefaultBlocking"

[requirements_traceability_gate]
mode = "DefaultBlocking"
"#;
        let config = BaEnforcementConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::DefaultBlocking
        );
        assert_eq!(
            config.requirements_traceability_mode,
            requirements_traceability_gate::ValidationMode::DefaultBlocking
        );
    }

    #[test]
    fn parses_strict_blocking() {
        let toml = r#"
[provenance_validate]
mode = "StrictBlocking"

[requirements_traceability_gate]
mode = "StrictBlocking"
"#;
        let config = BaEnforcementConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::StrictBlocking
        );
        assert_eq!(
            config.requirements_traceability_mode,
            requirements_traceability_gate::ValidationMode::StrictBlocking
        );
    }

    #[test]
    fn missing_keys_fall_back_to_observe_only() {
        let toml = "";
        let config = BaEnforcementConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::ObserveOnly
        );
        assert_eq!(
            config.requirements_traceability_mode,
            requirements_traceability_gate::ValidationMode::ObserveOnly
        );
    }

    #[test]
    fn missing_one_section_defaults_only_that_one() {
        // Operator sets provenance_validate to Strict but doesn't
        // mention requirements_traceability_gate → that one defaults.
        let toml = r#"
[provenance_validate]
mode = "StrictBlocking"
"#;
        let config = BaEnforcementConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::StrictBlocking
        );
        assert_eq!(
            config.requirements_traceability_mode,
            requirements_traceability_gate::ValidationMode::ObserveOnly
        );
    }

    #[test]
    fn unknown_mode_string_errors() {
        let toml = r#"
[provenance_validate]
mode = "TotallyMadeUp"
"#;
        let err = BaEnforcementConfig::from_toml_str(toml).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("TotallyMadeUp"));
        assert!(msg.contains("expected"));
    }

    #[test]
    fn malformed_toml_errors() {
        let err = BaEnforcementConfig::from_toml_str("not valid toml [[[").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse"));
    }

    #[test]
    fn with_shipped_and_overrides_missing_path_uses_shipped() {
        let config = BaEnforcementConfig::with_shipped_and_overrides(Some(Path::new(
            "/tmp/nonexistent-ba-enforcement-12345.toml",
        )))
        .unwrap();
        // Missing file → shipped (ObserveOnly).
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::ObserveOnly
        );
    }

    #[test]
    fn with_shipped_and_overrides_none_uses_shipped() {
        let config = BaEnforcementConfig::with_shipped_and_overrides(None).unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::ObserveOnly
        );
    }

    #[test]
    fn with_shipped_and_overrides_applies_operator_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ba-enforcement.toml");
        std::fs::write(
            &path,
            r#"
[provenance_validate]
mode = "DefaultBlocking"

[requirements_traceability_gate]
mode = "StrictBlocking"
"#,
        )
        .unwrap();
        let config = BaEnforcementConfig::with_shipped_and_overrides(Some(&path)).unwrap();
        assert_eq!(
            config.provenance_validate_mode,
            provenance_validate::ValidationMode::DefaultBlocking
        );
        assert_eq!(
            config.requirements_traceability_mode,
            requirements_traceability_gate::ValidationMode::StrictBlocking
        );
    }

    #[test]
    fn config_is_send_sync_copy() {
        fn assert_send_sync_copy<T: Send + Sync + Copy>() {}
        assert_send_sync_copy::<BaEnforcementConfig>();
    }
}
