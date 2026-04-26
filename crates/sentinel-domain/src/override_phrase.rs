//! Override-phrase domain predicates.
//!
//! Recognizes user prompts that opt-in to suspending sentinel guards
//! (hygiene checks, pre-commit verification, Doppler write protection).
//! Pure-string regex predicates with no IO; the hook layer is responsible
//! for what to *do* once a phrase matches (write a signed override token,
//! reset cooldown, etc.).
//!
//! Each phrase set is intentionally small and deliberately phrased:
//! catching every plausible variant invites accidental overrides ("can
//! we override the hygiene checks just for this commit?" said in passing).
//! High-friction language is the point.

use regex::Regex;

/// Phrases that opt the user out of `git_hygiene` / `commit_hygiene` checks.
pub const HYGIENE_OVERRIDE_PATTERNS: &[&str] = &[
    r"override\s+(hygiene|git|commit)",
    r"hygiene\s+override",
    r"force\s+continue",
    r"skip\s+hygiene",
];

/// Phrases that opt the user out of `pre_commit_verification`. Includes
/// "skip tests" because that's the most common natural way to phrase it.
pub const VERIFICATION_OVERRIDE_PATTERNS: &[&str] = &[
    r"override\s+verification",
    r"verification\s+override",
    r"skip\s+verification",
    r"skip\s+tests?",
    r"override\s+test",
];

/// Phrases that authorize a Doppler write/mutation. Doppler touches secrets,
/// so the language is intentionally explicit — generic "override" alone does
/// not match; the user has to name Doppler.
pub const DOPPLER_OVERRIDE_PATTERNS: &[&str] = &[
    r"override\s+doppler",
    r"doppler\s+override",
    r"allow\s+doppler\s+(write|writes|mutation|mutations)",
    r"authorize\s+doppler\s+(write|writes|mutation|mutations)",
];

/// True if `prompt` matches any pattern in [`HYGIENE_OVERRIDE_PATTERNS`].
#[must_use]
pub fn is_hygiene_override(prompt: &str) -> bool {
    matches_any(prompt, HYGIENE_OVERRIDE_PATTERNS)
}

/// True if `prompt` matches any pattern in [`VERIFICATION_OVERRIDE_PATTERNS`].
#[must_use]
pub fn is_verification_override(prompt: &str) -> bool {
    matches_any(prompt, VERIFICATION_OVERRIDE_PATTERNS)
}

/// True if `prompt` matches any pattern in [`DOPPLER_OVERRIDE_PATTERNS`].
#[must_use]
pub fn is_doppler_override(prompt: &str) -> bool {
    matches_any(prompt, DOPPLER_OVERRIDE_PATTERNS)
}

/// Common "match any of these regexes" helper. Returns `false` if a pattern
/// fails to compile — failures are silent because the patterns are static
/// and a bug would be caught by the unit tests in this module, not at runtime.
fn matches_any(prompt: &str, patterns: &[&str]) -> bool {
    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(prompt)).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── hygiene ──────────────────────────────────────────────────────

    #[test]
    fn hygiene_override_recognizes_each_pattern() {
        assert!(is_hygiene_override("override hygiene"));
        assert!(is_hygiene_override("override git"));
        assert!(is_hygiene_override("override commit"));
        assert!(is_hygiene_override("hygiene override"));
        assert!(is_hygiene_override("force continue"));
        assert!(is_hygiene_override("skip hygiene"));
    }

    #[test]
    fn hygiene_override_does_not_match_unrelated() {
        assert!(!is_hygiene_override("commit the fix"));
        assert!(!is_hygiene_override("override doppler"));
        assert!(!is_hygiene_override("verify this"));
    }

    // ─── verification ─────────────────────────────────────────────────

    #[test]
    fn verification_override_recognizes_each_pattern() {
        assert!(is_verification_override("override verification"));
        assert!(is_verification_override("verification override"));
        assert!(is_verification_override("skip verification"));
        assert!(is_verification_override("skip test"));
        assert!(is_verification_override("skip tests"));
        assert!(is_verification_override("override test"));
    }

    #[test]
    fn verification_override_does_not_match_unrelated() {
        assert!(!is_verification_override("run tests"));
        assert!(!is_verification_override("verify integration"));
    }

    // ─── doppler ──────────────────────────────────────────────────────

    #[test]
    fn doppler_override_requires_explicit_doppler() {
        // Generic override phrases don't match — Doppler protection is
        // intentionally high-friction.
        assert!(!is_doppler_override("override hygiene"));
        assert!(!is_doppler_override("force continue"));
        assert!(!is_doppler_override("authorize the change"));
    }

    #[test]
    fn doppler_override_recognizes_each_pattern() {
        assert!(is_doppler_override("override doppler"));
        assert!(is_doppler_override("doppler override"));
        assert!(is_doppler_override("allow doppler write"));
        assert!(is_doppler_override("allow doppler writes"));
        assert!(is_doppler_override("allow doppler mutation"));
        assert!(is_doppler_override("allow doppler mutations"));
        assert!(is_doppler_override("authorize doppler write"));
        assert!(is_doppler_override("authorize doppler writes"));
        assert!(is_doppler_override("authorize doppler mutation"));
        assert!(is_doppler_override("authorize doppler mutations"));
    }

    #[test]
    fn override_predicates_are_disjoint() {
        // A phrase that matches one override should not accidentally match
        // another. Pin this so future pattern edits don't cross-contaminate.
        let h = "override hygiene";
        assert!(is_hygiene_override(h));
        assert!(!is_verification_override(h));
        assert!(!is_doppler_override(h));

        let v = "skip tests";
        assert!(is_verification_override(v));
        assert!(!is_hygiene_override(v));
        assert!(!is_doppler_override(v));

        let d = "allow doppler writes";
        assert!(is_doppler_override(d));
        assert!(!is_hygiene_override(d));
        assert!(!is_verification_override(d));
    }
}
