//! Runtime data + TOML loader for [`crate::hooks::constitution_gate`].
//!
//! Lives in the application layer so the hook (which depends on
//! `Rule`) doesn't need a separate path-dep on
//! `sentinel-infrastructure`. The TOML parsing has no I/O — just
//! string → struct — so the application layer is the right home.
//! Actual file reading from `~/.claude/sentinel/config/...`
//! happens in `sentinel-infrastructure` (which calls
//! [`ConstitutionGateConfig::from_toml_str`]).
//!
//! Schema:
//!
//! ```toml
//! [[rule]]
//! name = "consul-domain-purity"
//! path_prefix = "crates/consul-domain/"
//! path_suffix = ".rs"             # optional
//! banned_patterns = ["sqlx::", "tokio::net::"]
//! reason = "Constitution Rule 1: consul-domain has zero I/O"
//! citation = ".specify/memory/constitution.md#rule-1"  # optional
//!
//! [[rule]]
//! name = "consul-protocol-vendor-free"
//! path_prefix = "crates/consul-protocol/"
//! banned_patterns = ["anthropic", "openai"]
//! reason = "Constitution Rule 2: no vendor names in consul-protocol"
//! ```
//!
//! Path matching is intentionally simple: prefix + optional
//! suffix. No glob crate dep; covers the common case (a crate
//! root) without pulling in a transitive dependency.

use anyhow::{Context, Result};
use serde::Deserialize;

/// One enforceable rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Operator-readable rule id (appears in deny messages).
    pub name: String,
    /// Path prefix — a write to a path that starts with this
    /// string is considered "within scope" of this rule.
    pub path_prefix: String,
    /// Optional path suffix — if set, the path must also end
    /// with this string (e.g. `.rs` to skip docs and TOML).
    pub path_suffix: Option<String>,
    /// Literal substrings that MUST NOT appear in new content
    /// landing at a path matched by `path_prefix`/`path_suffix`.
    pub banned_patterns: Vec<String>,
    /// Operator-facing explanation (appears in deny messages).
    pub reason: String,
    /// Optional citation (`constitution.md#rule-1`, ADR ref, …).
    pub citation: Option<String>,
}

impl Rule {
    /// True when `path` is in this rule's scope.
    #[must_use]
    pub fn matches_path(&self, path: &str) -> bool {
        if !path.starts_with(&self.path_prefix) {
            return false;
        }
        match &self.path_suffix {
            Some(suffix) => path.ends_with(suffix.as_str()),
            None => true,
        }
    }

    /// Return the first banned pattern that appears in `content`,
    /// or `None` if the content is clean.
    #[must_use]
    pub fn find_banned(&self, content: &str) -> Option<String> {
        self.banned_patterns
            .iter()
            .find(|pat| content.contains(pat.as_str()))
            .cloned()
    }
}

/// Top-level TOML document.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConstitutionGateConfig {
    /// All enforceable rules. Empty = no-op gate.
    pub rules: Vec<Rule>,
}

impl ConstitutionGateConfig {
    /// Parse from a TOML string. An empty document yields an
    /// empty config (the gate becomes a no-op). Malformed TOML
    /// surfaces as `Err` so operators see the typo at startup,
    /// not in production when a write silently slips past.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let doc: TomlDoc = toml::from_str(s).context("failed to parse constitution-gate TOML")?;
        let rules = doc.rule.unwrap_or_default();
        for r in &rules {
            if r.name.trim().is_empty() {
                anyhow::bail!("constitution-gate: rule with empty name is not allowed");
            }
            if r.path_prefix.trim().is_empty() {
                anyhow::bail!("constitution-gate: rule {:?} has empty path_prefix", r.name);
            }
            if r.banned_patterns.is_empty() {
                anyhow::bail!(
                    "constitution-gate: rule {:?} has no banned_patterns — would never fire",
                    r.name,
                );
            }
        }
        Ok(Self {
            rules: rules
                .into_iter()
                .map(|r| Rule {
                    name: r.name,
                    path_prefix: r.path_prefix,
                    path_suffix: r.path_suffix,
                    banned_patterns: r.banned_patterns,
                    reason: r.reason,
                    citation: r.citation,
                })
                .collect(),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct TomlDoc {
    #[serde(default)]
    rule: Option<Vec<TomlRule>>,
}

#[derive(Debug, Deserialize)]
struct TomlRule {
    name: String,
    path_prefix: String,
    #[serde(default)]
    path_suffix: Option<String>,
    banned_patterns: Vec<String>,
    reason: String,
    #[serde(default)]
    citation: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_empty_rules() {
        let cfg = ConstitutionGateConfig::from_toml_str("").unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn parses_a_complete_rule() {
        let cfg = ConstitutionGateConfig::from_toml_str(
            r#"
            [[rule]]
            name = "consul-domain-purity"
            path_prefix = "crates/consul-domain/"
            path_suffix = ".rs"
            banned_patterns = ["sqlx::", "tokio::net::"]
            reason = "Constitution Rule 1"
            citation = ".specify/memory/constitution.md#rule-1"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.rules.len(), 1);
        let r = &cfg.rules[0];
        assert_eq!(r.name, "consul-domain-purity");
        assert_eq!(r.path_suffix.as_deref(), Some(".rs"));
        assert_eq!(r.banned_patterns, vec!["sqlx::", "tokio::net::"]);
        assert!(r.citation.is_some());
    }

    #[test]
    fn citation_and_suffix_are_optional() {
        let cfg = ConstitutionGateConfig::from_toml_str(
            r#"
            [[rule]]
            name = "no-vendor"
            path_prefix = "crates/consul-protocol/"
            banned_patterns = ["anthropic"]
            reason = "Constitution Rule 2"
            "#,
        )
        .unwrap();
        let r = &cfg.rules[0];
        assert_eq!(r.path_suffix, None);
        assert_eq!(r.citation, None);
    }

    #[test]
    fn empty_name_rejected() {
        let err = ConstitutionGateConfig::from_toml_str(
            r#"
            [[rule]]
            name = ""
            path_prefix = "crates/foo/"
            banned_patterns = ["x"]
            reason = "ok"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty name"));
    }

    #[test]
    fn empty_banned_patterns_rejected() {
        let err = ConstitutionGateConfig::from_toml_str(
            r#"
            [[rule]]
            name = "would-never-fire"
            path_prefix = "crates/foo/"
            banned_patterns = []
            reason = "ok"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("would never fire"));
    }

    #[test]
    fn matches_path_honors_prefix_and_suffix() {
        let r = Rule {
            name: "x".into(),
            path_prefix: "crates/consul-domain/".into(),
            path_suffix: Some(".rs".into()),
            banned_patterns: vec!["sqlx::".into()],
            reason: "y".into(),
            citation: None,
        };
        assert!(r.matches_path("crates/consul-domain/src/lib.rs"));
        assert!(!r.matches_path("crates/consul-domain/README.md"));
        assert!(!r.matches_path("crates/consul-storage/src/lib.rs"));
    }

    #[test]
    fn find_banned_returns_first_hit() {
        let r = Rule {
            name: "x".into(),
            path_prefix: "p/".into(),
            path_suffix: None,
            banned_patterns: vec!["sqlx::".into(), "reqwest::".into()],
            reason: "y".into(),
            citation: None,
        };
        let hit = r
            .find_banned("use sqlx::Pool;\nuse reqwest::Client;")
            .unwrap();
        assert_eq!(hit, "sqlx::");
        assert!(r.find_banned("use serde::Deserialize;").is_none());
    }
}
