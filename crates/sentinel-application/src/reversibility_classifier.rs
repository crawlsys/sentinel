//! Static (in-memory) implementations of
//! [`ReversibilityClassifierPort`](sentinel_domain::ports::ReversibilityClassifierPort).
//!
//! [`StaticReversibilityClassifier`] is the application-layer test helper for
//! the A6 design (`docs/a6-reversibility-graded-tripwires.md`). Equivalent
//! role to [`MockClock`](crate::dedupe::MockClock) for [`Clock`]: hooks
//! that take a `&dyn ReversibilityClassifierPort` use this in their unit
//! tests, with the production adapter (Phase 3b in
//! `sentinel-infrastructure`) doing the real TOML-loaded four-layer
//! evaluation.
//!
//! [`Clock`]: sentinel_domain::clock::Clock

use std::collections::HashMap;

use sentinel_domain::ports::ReversibilityClassifierPort;
use sentinel_domain::ReversibilityClass;

/// In-memory [`ReversibilityClassifierPort`] backed by a static map from
/// tool name to class.
///
/// Lookup is exact-match on `tool_name`. When no entry exists, the
/// classifier returns its configured default class (see
/// [`Self::with_default`]). The default-of-defaults is
/// [`ReversibilityClass::Irreversible`] so behavior matches the port's
/// "unknown tools should default conservatively" contract.
///
/// `tool_input` is ignored by this classifier — pattern-level rules
/// (e.g. Bash command patterns from the A6 spec's Layer 3) are out of
/// scope for the static test helper. The production adapter in
/// `sentinel-infrastructure` (Phase 3b) handles input-dependent
/// classification; static tests can compose multiple classifiers if
/// they need pattern coverage.
#[derive(Debug, Clone)]
pub struct StaticReversibilityClassifier {
    table: HashMap<String, ReversibilityClass>,
    default: ReversibilityClass,
}

impl StaticReversibilityClassifier {
    /// Construct from a `tool_name → class` map. Unknown tools fall
    /// back to [`ReversibilityClass::Irreversible`] (the conservative
    /// default the port trait specifies).
    #[must_use]
    pub fn new(table: HashMap<String, ReversibilityClass>) -> Self {
        Self {
            table,
            default: ReversibilityClass::Irreversible,
        }
    }

    /// Construct an empty classifier where every tool is unknown and
    /// therefore returns the default.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(HashMap::new())
    }

    /// Replace the default class used for unknown tools.
    ///
    /// Defaults to [`ReversibilityClass::Irreversible`]; tests that want
    /// "everything is trivially reversible unless explicitly listed"
    /// (e.g., to focus on a single Catastrophic case) can set the
    /// fallback explicitly.
    #[must_use]
    pub fn with_default(mut self, default: ReversibilityClass) -> Self {
        self.default = default;
        self
    }

    /// Insert or replace a tool's class in-place. Returns `self` for
    /// builder-style chaining in tests.
    #[must_use]
    pub fn with(mut self, tool_name: impl Into<String>, class: ReversibilityClass) -> Self {
        self.table.insert(tool_name.into(), class);
        self
    }
}

impl ReversibilityClassifierPort for StaticReversibilityClassifier {
    fn classify(
        &self,
        tool_name: &str,
        _tool_input: &serde_json::Value,
    ) -> ReversibilityClass {
        self.table
            .get(tool_name)
            .copied()
            .unwrap_or(self.default)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn no_input() -> serde_json::Value {
        json!({})
    }

    #[test]
    fn classifies_known_tools_from_table() {
        let classifier = StaticReversibilityClassifier::empty()
            .with("Read", ReversibilityClass::TriviallyReversible)
            .with("Edit", ReversibilityClass::ReversibleWithEffort)
            .with("Bash", ReversibilityClass::ReversibleWithEffort);

        assert_eq!(
            classifier.classify("Read", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            classifier.classify("Edit", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
        assert_eq!(
            classifier.classify("Bash", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn unknown_tool_falls_back_to_default() {
        let classifier = StaticReversibilityClassifier::empty();
        assert_eq!(
            classifier.classify("UnknownTool", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn default_of_defaults_is_irreversible_per_port_contract() {
        let classifier = StaticReversibilityClassifier::empty();
        // Even with no entries, the conservative default protects against
        // unknown tools slipping through unclassified.
        assert_eq!(
            classifier.classify("anything", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn with_default_overrides_fallback() {
        let classifier = StaticReversibilityClassifier::empty()
            .with_default(ReversibilityClass::TriviallyReversible);
        assert_eq!(
            classifier.classify("anything", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn explicit_entry_beats_default() {
        let classifier = StaticReversibilityClassifier::empty()
            .with_default(ReversibilityClass::TriviallyReversible)
            .with("Bash", ReversibilityClass::Catastrophic);
        assert_eq!(
            classifier.classify("Bash", &no_input()),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            classifier.classify("OtherTool", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn new_from_hashmap_directly() {
        let mut map = HashMap::new();
        map.insert("Foo".to_string(), ReversibilityClass::Catastrophic);
        let classifier = StaticReversibilityClassifier::new(map);
        assert_eq!(
            classifier.classify("Foo", &no_input()),
            ReversibilityClass::Catastrophic
        );
    }

    #[test]
    fn with_builder_supports_chaining_across_classes() {
        let classifier = StaticReversibilityClassifier::empty()
            .with("Read", ReversibilityClass::TriviallyReversible)
            .with("Write", ReversibilityClass::ReversibleWithEffort)
            .with("mcp__gmail__send", ReversibilityClass::Catastrophic)
            .with("mcp__linear__list_issues", ReversibilityClass::TriviallyReversible);

        assert_eq!(
            classifier.classify("Read", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
        assert_eq!(
            classifier.classify("Write", &no_input()),
            ReversibilityClass::ReversibleWithEffort
        );
        assert_eq!(
            classifier.classify("mcp__gmail__send", &no_input()),
            ReversibilityClass::Catastrophic
        );
        assert_eq!(
            classifier.classify("mcp__linear__list_issues", &no_input()),
            ReversibilityClass::TriviallyReversible
        );
    }

    #[test]
    fn ignores_tool_input() {
        // Static classifier looks only at tool_name. Different inputs to
        // the same tool produce the same class.
        let classifier = StaticReversibilityClassifier::empty()
            .with("Bash", ReversibilityClass::ReversibleWithEffort);

        let benign = json!({ "command": "ls" });
        let scary = json!({ "command": "rm -rf /" });

        assert_eq!(
            classifier.classify("Bash", &benign),
            ReversibilityClass::ReversibleWithEffort
        );
        // Despite the dangerous input, the static classifier doesn't
        // upgrade — pattern-level rules are out of scope here.
        assert_eq!(
            classifier.classify("Bash", &scary),
            ReversibilityClass::ReversibleWithEffort
        );
    }

    #[test]
    fn usable_through_port_trait_object() {
        // Compile-time check + minimal behavior: hooks accept
        // `&dyn ReversibilityClassifierPort`, so the static helper must
        // be usable that way.
        let classifier = StaticReversibilityClassifier::empty()
            .with("X", ReversibilityClass::Irreversible);
        let port: &dyn ReversibilityClassifierPort = &classifier;
        assert_eq!(
            port.classify("X", &no_input()),
            ReversibilityClass::Irreversible
        );
    }

    #[test]
    fn classifier_is_send_sync() {
        // Compile-time check that the type satisfies the port's
        // Send + Sync bound. If a non-Send/Sync field sneaks in later,
        // this fn will fail to compile.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StaticReversibilityClassifier>();
    }
}
