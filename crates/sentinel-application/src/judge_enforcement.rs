//! Judge enforcement mode — the shadow → warn → enforce rollout switch.
//!
//! Read from `SENTINEL_JUDGE_ENFORCEMENT` (default [`Mode::Shadow`]). Governs
//! what happens with the verdict the `step_judge` PostToolUse hook produces:
//!
//! - [`Mode::Shadow`] (default): the judge runs and its verdict is recorded,
//!   but nothing blocks. Safe to ship — observe the pass/over-block mix on real
//!   work before tightening.
//! - [`Mode::Warn`]: a non-sufficient verdict surfaces a visible warning, but
//!   the step still seals.
//! - [`Mode::Enforce`]: a non-sufficient verdict blocks `submit_step_complete`
//!   from sealing (the structural gate the proof-chain architecture intends).
//!
//! Two call sites read this: the `step_judge` hook (warn-vs-silent on a
//! non-sufficient verdict) and `submit_step_complete` (whether a non-sufficient
//! verdict blocks the seal). Reading the same env var in both keeps them
//! consistent without threading a config object through the hook plumbing.
//!
//! **Why default Shadow:** auto-judging every step is a new behavior. The
//! pressure test (`live_judge_pressure`) proved the recalibrated judge passes
//! genuinely-sufficient work, but real-world evidence is messier than fixtures.
//! Shadow lets operators watch the verdict distribution before promoting to
//! enforce, so a calibration miss can't brick the workflow.

use std::fmt;

/// Judge enforcement rollout mode. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Run the judge, record the verdict, never block. Default.
    #[default]
    Shadow,
    /// Non-sufficient verdict warns but still seals.
    Warn,
    /// Non-sufficient verdict blocks the seal.
    Enforce,
}

impl Mode {
    /// Environment variable consulted by [`Mode::from_env`].
    pub const ENV_VAR: &'static str = "SENTINEL_JUDGE_ENFORCEMENT";

    /// Parse a mode from a string. Case-insensitive; unknown values fall
    /// back to [`Mode::Shadow`] (fail-safe-open for the rollout switch —
    /// an unrecognized value must never silently start blocking work).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "warn" => Self::Warn,
            "enforce" => Self::Enforce,
            // "shadow", empty, or anything unrecognized → Shadow.
            _ => Self::Shadow,
        }
    }

    /// Read the mode from `SENTINEL_JUDGE_ENFORCEMENT` via the supplied
    /// resolver. Absent → [`Mode::Shadow`]. The resolver seam keeps this
    /// testable without process-wide env mutation (Rust 2024 marks
    /// `set_var` unsafe).
    #[must_use]
    pub fn from_env_with<F>(env: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        env(Self::ENV_VAR).map_or(Self::Shadow, |v| Self::parse(&v))
    }

    /// Read the mode from the real process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_env_with(|k| std::env::var(k).ok())
    }

    /// Does a non-sufficient verdict block the seal in this mode?
    #[must_use]
    pub const fn blocks_seal(self) -> bool {
        matches!(self, Self::Enforce)
    }

    /// Does a non-sufficient verdict surface a warning in this mode?
    /// (Both Warn and Enforce warn; Shadow is silent.)
    #[must_use]
    pub const fn warns(self) -> bool {
        matches!(self, Self::Warn | Self::Enforce)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Shadow => "shadow",
            Self::Warn => "warn",
            Self::Enforce => "enforce",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_shadow() {
        assert_eq!(Mode::default(), Mode::Shadow);
    }

    #[test]
    fn parse_known_values_case_insensitive() {
        assert_eq!(Mode::parse("warn"), Mode::Warn);
        assert_eq!(Mode::parse("WARN"), Mode::Warn);
        assert_eq!(Mode::parse(" Enforce "), Mode::Enforce);
        assert_eq!(Mode::parse("shadow"), Mode::Shadow);
    }

    #[test]
    fn unknown_and_empty_fall_back_to_shadow() {
        // Fail-safe: a typo must never silently start blocking work.
        assert_eq!(Mode::parse("blokc"), Mode::Shadow);
        assert_eq!(Mode::parse(""), Mode::Shadow);
        assert_eq!(Mode::parse("ENFORCE_NOW_PLZ"), Mode::Shadow);
    }

    #[test]
    fn from_env_absent_is_shadow() {
        let env = |_: &str| None;
        assert_eq!(Mode::from_env_with(env), Mode::Shadow);
    }

    #[test]
    fn from_env_reads_the_var() {
        let env = |k: &str| (k == Mode::ENV_VAR).then(|| "enforce".to_string());
        assert_eq!(Mode::from_env_with(env), Mode::Enforce);
    }

    #[test]
    fn blocks_seal_only_in_enforce() {
        assert!(!Mode::Shadow.blocks_seal());
        assert!(!Mode::Warn.blocks_seal());
        assert!(Mode::Enforce.blocks_seal());
    }

    #[test]
    fn warns_in_warn_and_enforce_not_shadow() {
        assert!(!Mode::Shadow.warns());
        assert!(Mode::Warn.warns());
        assert!(Mode::Enforce.warns());
    }

    #[test]
    fn display_roundtrips_through_parse() {
        for m in [Mode::Shadow, Mode::Warn, Mode::Enforce] {
            assert_eq!(Mode::parse(&m.to_string()), m);
        }
    }
}
