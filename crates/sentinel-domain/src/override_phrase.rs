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

/// Phrases that opt the user out of the `phase_gate` protection on
/// `~/.claude/skills/**/SKILL.md` and `~/.claude/skills/**/phases/**`.
/// Lets the user perform marketplace-wide skill refactors without
/// tripping the "phase file modification" or "skill definition file"
/// block. **Does NOT suspend protection on `~/.claude/sentinel/`,
/// settings.json, or hooks.toml** — those remain blocked always.
///
/// Language is explicit: the user has to name "phase", "skills", or
/// "refactor" so a passing mention of "override" in unrelated work
/// won't unlock skill edits.
pub const PHASE_GATE_OVERRIDE_PATTERNS: &[&str] = &[
    r"override\s+(phase\s+gate|phase|skills?)",
    r"(phase\s+gate|skills?)\s+override",
    r"refactor\s+skills?",
    r"allow\s+(skill|phase)\s+(edit|edits|modification|modifications)",
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

/// True if `prompt` matches any pattern in [`PHASE_GATE_OVERRIDE_PATTERNS`].
/// When true, the hygiene_override hook will write a signed phase-gate
/// override token; phase_gate.rs will let writes to skill files through
/// for the override TTL (60s).
#[must_use]
pub fn is_phase_gate_override(prompt: &str) -> bool {
    matches_any(prompt, PHASE_GATE_OVERRIDE_PATTERNS)
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
        assert!(!is_phase_gate_override(h));

        let v = "skip tests";
        assert!(is_verification_override(v));
        assert!(!is_hygiene_override(v));
        assert!(!is_doppler_override(v));
        assert!(!is_phase_gate_override(v));

        let d = "allow doppler writes";
        assert!(is_doppler_override(d));
        assert!(!is_hygiene_override(d));
        assert!(!is_verification_override(d));
        assert!(!is_phase_gate_override(d));

        let p = "refactor skills";
        assert!(is_phase_gate_override(p));
        assert!(!is_hygiene_override(p));
        assert!(!is_verification_override(p));
        assert!(!is_doppler_override(p));
    }

    // ─── phase gate ───────────────────────────────────────────────────

    #[test]
    fn phase_gate_override_recognizes_each_pattern() {
        assert!(is_phase_gate_override("override phase gate"));
        assert!(is_phase_gate_override("override phase"));
        assert!(is_phase_gate_override("override skills"));
        assert!(is_phase_gate_override("override skill"));
        assert!(is_phase_gate_override("phase gate override"));
        assert!(is_phase_gate_override("skills override"));
        assert!(is_phase_gate_override("skill override"));
        assert!(is_phase_gate_override("refactor skills"));
        assert!(is_phase_gate_override("refactor skill"));
        assert!(is_phase_gate_override("allow skill edit"));
        assert!(is_phase_gate_override("allow skill edits"));
        assert!(is_phase_gate_override("allow phase modification"));
        assert!(is_phase_gate_override("allow phase modifications"));
    }

    #[test]
    fn phase_gate_override_does_not_match_unrelated() {
        // Generic "override" without phase/skill naming should not match
        assert!(!is_phase_gate_override("override hygiene"));
        assert!(!is_phase_gate_override("override doppler"));
        assert!(!is_phase_gate_override("override verification"));
        // Unrelated mentions of "skills"
        assert!(!is_phase_gate_override("good engineering skills"));
        assert!(!is_phase_gate_override("the refactor team"));
    }
}
