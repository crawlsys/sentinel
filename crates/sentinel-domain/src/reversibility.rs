//! Reversibility classification for actions.
//!
//! Per the A6 design (`docs/a6-reversibility-graded-tripwires.md`), every
//! blast-radius gate in sentinel queries a single shared axis: the
//! reversibility class of the action being gated. This module defines the
//! four-value enum that consumers reason about. Subsequent phases add the
//! `ReversibilityClassifierPort` trait and the layered classifier adapter
//! that populates a class for a given (tool, input) pair.
//!
//! The ordering of variants is load-bearing: `TriviallyReversible <
//! ReversibleWithEffort < Irreversible < Catastrophic`. Derived `PartialOrd`
//! exposes [`ReversibilityClass::at_least`] and the comparison operators
//! consumers use to write rules like "fire dry-run-then-commit (A3) when
//! class is at least Irreversible."

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Reversibility class of an action.
///
/// Variants are ordered by severity, lowest first. Derived `PartialOrd`
/// uses declaration order, so `Catastrophic > Irreversible >
/// ReversibleWithEffort > TriviallyReversible`. The serde representation
/// matches the variant name verbatim (`"TriviallyReversible"`, etc.) so
/// TOML config files in `config/reversibility.toml` read naturally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReversibilityClass {
    /// Undoable in seconds; no human attention required. File saves under
    /// VCS, memory notes, plan files, read-only ops. Blast-radius gates
    /// are silent for this class.
    TriviallyReversible,
    /// Undoable with a known recovery procedure. Source `Edit`/`Write`
    /// inside a worktree, `git commit`, schema migrations with rollback,
    /// mutating MCP scoped to operator-owned resources. Standard gate
    /// stack applies; no dry-run-then-commit auditor required.
    ReversibleWithEffort,
    /// Practically undoable; state changes visible outside the operator's
    /// local control. `git push` to shared branches, production deploys,
    /// sent emails, posted PRs, published BA briefs. A3 dry-run-then-commit
    /// fires; auditor scores; human spot-check sampling at the configured
    /// rate.
    Irreversible,
    /// Irreversible AND high blast radius. Production DB drops, account
    /// deletion, financial transactions, force-push to `main`, exec-deck
    /// delivery in the BA vertical. A3 fires AND human is always sampled
    /// regardless of auditor result; two-eyes rule applies for BA outputs.
    Catastrophic,
}

impl ReversibilityClass {
    /// Return `true` if `self` is at least as severe as `other`.
    ///
    /// Convenience for the canonical gate condition:
    ///
    /// ```text
    /// if class.at_least(ReversibilityClass::Irreversible) {
    ///     // fire dry-run-then-commit (A3)
    /// }
    /// ```
    #[must_use]
    pub fn at_least(self, other: Self) -> bool {
        self >= other
    }
}

impl fmt::Display for ReversibilityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::TriviallyReversible => "TriviallyReversible",
            Self::ReversibleWithEffort => "ReversibleWithEffort",
            Self::Irreversible => "Irreversible",
            Self::Catastrophic => "Catastrophic",
        };
        f.write_str(s)
    }
}

/// Error returned by [`ReversibilityClass::from_str`] for unknown input.
///
/// Carries the invalid input verbatim so callers can surface a precise
/// message to operators editing `config/reversibility.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseReversibilityClassError {
    invalid_input: String,
}

impl ParseReversibilityClassError {
    /// The input string that failed to parse.
    #[must_use]
    pub fn invalid_input(&self) -> &str {
        &self.invalid_input
    }
}

impl fmt::Display for ParseReversibilityClassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown reversibility class `{}`; expected one of TriviallyReversible, ReversibleWithEffort, Irreversible, Catastrophic",
            self.invalid_input
        )
    }
}

impl std::error::Error for ParseReversibilityClassError {}

impl FromStr for ReversibilityClass {
    type Err = ParseReversibilityClassError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "TriviallyReversible" => Ok(Self::TriviallyReversible),
            "ReversibleWithEffort" => Ok(Self::ReversibleWithEffort),
            "Irreversible" => Ok(Self::Irreversible),
            "Catastrophic" => Ok(Self::Catastrophic),
            _ => Err(ParseReversibilityClassError {
                invalid_input: s.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn ordering_increases_with_severity() {
        assert!(ReversibilityClass::TriviallyReversible < ReversibilityClass::ReversibleWithEffort);
        assert!(ReversibilityClass::ReversibleWithEffort < ReversibilityClass::Irreversible);
        assert!(ReversibilityClass::Irreversible < ReversibilityClass::Catastrophic);
    }

    #[test]
    fn at_least_matches_ge_inclusive() {
        // strictly greater
        assert!(ReversibilityClass::Catastrophic.at_least(ReversibilityClass::Irreversible));
        // equal
        assert!(ReversibilityClass::Irreversible.at_least(ReversibilityClass::Irreversible));
        // strictly less
        assert!(
            !ReversibilityClass::ReversibleWithEffort.at_least(ReversibilityClass::Irreversible)
        );
        assert!(!ReversibilityClass::TriviallyReversible
            .at_least(ReversibilityClass::ReversibleWithEffort));
    }

    #[test]
    fn at_least_models_a3_canonical_trigger() {
        // A3 fires when class is at least Irreversible
        let trigger_for = |c: ReversibilityClass| c.at_least(ReversibilityClass::Irreversible);
        assert!(!trigger_for(ReversibilityClass::TriviallyReversible));
        assert!(!trigger_for(ReversibilityClass::ReversibleWithEffort));
        assert!(trigger_for(ReversibilityClass::Irreversible));
        assert!(trigger_for(ReversibilityClass::Catastrophic));
    }

    #[test]
    fn display_matches_variant_name() {
        assert_eq!(
            ReversibilityClass::TriviallyReversible.to_string(),
            "TriviallyReversible"
        );
        assert_eq!(
            ReversibilityClass::ReversibleWithEffort.to_string(),
            "ReversibleWithEffort"
        );
        assert_eq!(ReversibilityClass::Irreversible.to_string(), "Irreversible");
        assert_eq!(ReversibilityClass::Catastrophic.to_string(), "Catastrophic");
    }

    #[test]
    fn from_str_round_trips_with_display_for_all_variants() {
        for class in [
            ReversibilityClass::TriviallyReversible,
            ReversibilityClass::ReversibleWithEffort,
            ReversibilityClass::Irreversible,
            ReversibilityClass::Catastrophic,
        ] {
            let s = class.to_string();
            let parsed: ReversibilityClass = s.parse().expect("should parse");
            assert_eq!(parsed, class);
        }
    }

    #[test]
    fn from_str_rejects_unknown_with_helpful_message() {
        let err = "Unknown".parse::<ReversibilityClass>().unwrap_err();
        assert_eq!(err.invalid_input(), "Unknown");
        let msg = err.to_string();
        assert!(
            msg.contains("Unknown"),
            "message should quote the input: {msg}"
        );
        assert!(
            msg.contains("Catastrophic"),
            "message should list valid options: {msg}"
        );
    }

    #[test]
    fn from_str_is_case_sensitive() {
        assert!("catastrophic".parse::<ReversibilityClass>().is_err());
        assert!("CATASTROPHIC".parse::<ReversibilityClass>().is_err());
        assert!("Catastrophic".parse::<ReversibilityClass>().is_ok());
    }

    #[test]
    fn from_str_rejects_empty_string() {
        let err = "".parse::<ReversibilityClass>().unwrap_err();
        assert_eq!(err.invalid_input(), "");
    }

    #[test]
    fn serde_round_trips_via_json() {
        let class = ReversibilityClass::Catastrophic;
        let json = serde_json::to_string(&class).expect("serialize");
        assert_eq!(json, "\"Catastrophic\"");
        let parsed: ReversibilityClass = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, class);
    }

    #[test]
    fn serde_round_trips_all_variants() {
        for class in [
            ReversibilityClass::TriviallyReversible,
            ReversibilityClass::ReversibleWithEffort,
            ReversibilityClass::Irreversible,
            ReversibilityClass::Catastrophic,
        ] {
            let json = serde_json::to_string(&class).expect("serialize");
            let parsed: ReversibilityClass = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, class);
        }
    }

    #[test]
    fn serde_rejects_unknown_variant_in_json() {
        let bad = "\"Unknown\"";
        let result: Result<ReversibilityClass, _> = serde_json::from_str(bad);
        assert!(result.is_err());
    }

    #[test]
    fn enum_is_copy_so_consumers_dont_borrow() {
        // Compile-time check: type is Copy. If derive(Copy) is removed,
        // the second binding fails to compile (value moved).
        let class = ReversibilityClass::Irreversible;
        let a = class;
        let b = class;
        assert_eq!(a, b);
    }

    #[test]
    fn hash_consistent_with_eq() {
        let mut set = HashSet::new();
        assert!(set.insert(ReversibilityClass::Catastrophic));
        assert!(!set.insert(ReversibilityClass::Catastrophic)); // dedup
        assert!(set.insert(ReversibilityClass::Irreversible));
        assert_eq!(set.len(), 2);
    }
}
