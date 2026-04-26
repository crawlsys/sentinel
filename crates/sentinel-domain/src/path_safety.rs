//! Path-segment safety predicates.
//!
//! Rules for validating path components (skill names, phase file names)
//! before they're used to construct filesystem paths or compared against
//! a canonical form.

/// Maximum length (in chars) of a single path segment.
pub const SAFE_NAME_MAX_LEN: usize = 64;

/// Validate that a path segment contains only safe ASCII characters.
///
/// Allows: `a-z`, `A-Z`, `0-9`, hyphen `-`, underscore `_`, dot `.`
/// (for the `.md` extension on phase file names).
///
/// **Explicitly rejects ALL non-ASCII characters**, including Unicode
/// confusables (e.g. Cyrillic `а` U+0430 vs Latin `a` U+0061). The
/// confusable-character class is the homoglyph-attack surface: a skill
/// directory named `linеar` (Cyrillic `е`) would match a string-literal
/// comparison against `linear` only if the comparison ignored Unicode.
/// This predicate refuses to look — non-ASCII is a hard reject.
#[must_use]
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= SAFE_NAME_MAX_LEN
        && name.is_ascii()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_skill_names() {
        assert!(is_safe_name("linear"));
        assert!(is_safe_name("ddd-hexagonal"));
        assert!(is_safe_name("memory_bank"));
        assert!(is_safe_name("v1.2.3"));
        assert!(is_safe_name("claim.md"));
        assert!(is_safe_name("a"));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_safe_name(""));
    }

    #[test]
    fn rejects_too_long() {
        let too_long = "a".repeat(SAFE_NAME_MAX_LEN + 1);
        assert!(!is_safe_name(&too_long));
        // Boundary: exactly SAFE_NAME_MAX_LEN is OK.
        let exact = "a".repeat(SAFE_NAME_MAX_LEN);
        assert!(is_safe_name(&exact));
    }

    #[test]
    fn rejects_homoglyph_attack() {
        // Cyrillic 'а' U+0430 looks like Latin 'a' but is not ASCII.
        assert!(!is_safe_name("lin\u{0430}r"));
        // Greek 'ο' U+03BF looks like Latin 'o'.
        assert!(!is_safe_name("link\u{03BF}"));
        // Full-width Latin letters (CJK forms).
        assert!(!is_safe_name("\u{FF41}"));
    }

    #[test]
    fn rejects_path_traversal_chars() {
        assert!(!is_safe_name("../etc"));
        assert!(!is_safe_name("foo/bar"));
        assert!(!is_safe_name("foo\\bar"));
    }

    #[test]
    fn rejects_whitespace_and_control() {
        assert!(!is_safe_name("foo bar"));
        assert!(!is_safe_name("foo\tbar"));
        assert!(!is_safe_name("foo\nbar"));
    }

    #[test]
    fn rejects_other_punctuation() {
        assert!(!is_safe_name("foo:bar"));
        assert!(!is_safe_name("foo;bar"));
        assert!(!is_safe_name("foo*bar"));
        assert!(!is_safe_name("foo!"));
    }
}
