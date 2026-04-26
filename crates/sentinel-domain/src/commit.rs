//! Commit-message domain predicates.
//!
//! Pure rules for classifying git commit messages — extracted from the
//! `commit_message_validator` hook so they can be reused (and tested) without
//! a hook context. The hook keeps the orchestration (parse the bash command,
//! load project config, decide what to block); the rules themselves live here.

use regex::Regex;

/// Conventional-commit prefixes recognized by the validator.
///
/// Source of truth — the hook's `VALID_PREFIXES` was previously a sibling
/// const; this is the single canonical list.
pub const VALID_PREFIXES: &[&str] = &[
    "feat", "fix", "chore", "docs", "refactor", "test", "style", "perf", "ci",
    "build", "revert",
];

/// Check whether a commit message subject (first line) follows the
/// conventional-commit format `type(scope?): description`.
///
/// Returns `false` for:
/// - empty messages
/// - subjects without a `:` separator
/// - subjects whose `type` is not in [`VALID_PREFIXES`]
///
/// The body of a multi-line message is ignored; only the subject is checked.
#[must_use]
pub fn is_conventional(message: &str) -> bool {
    let subject = message.lines().next().unwrap_or(message).trim();
    if subject.is_empty() {
        return false;
    }
    // Type-(optional scope)-colon-space-description.
    let prefix_re = match Regex::new(r"^(\w+)(?:\([^)]*\))?:\s*.+") {
        Ok(re) => re,
        Err(_) => return false,
    };
    let caps = match prefix_re.captures(subject) {
        Some(c) => c,
        None => return false,
    };
    let prefix = caps[1].to_lowercase();
    VALID_PREFIXES.contains(&prefix.as_str())
}

/// Check whether `message` references a Linear issue from any of the given
/// project prefixes (e.g. `["FPCRM", "FPFIELD"]`).
///
/// Match is case-insensitive and anchored to word boundaries: `FPCRM-42`,
/// `fpcrm-42`, and `FpCrm-42` all match prefix `"FPCRM"`. The numeric tail
/// must be one or more digits.
///
/// `prefixes` is borrowed as `&[String]` to match the existing on-disk
/// project-config shape; an empty list returns `false`.
#[must_use]
pub fn has_linear_ref(message: &str, prefixes: &[String]) -> bool {
    for prefix in prefixes {
        let pat = format!(r"(?i)\b{}-\d+\b", regex::escape(prefix));
        if let Ok(re) = Regex::new(&pat) {
            if re.is_match(message) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── is_conventional ───────────────────────────────────────────────

    #[test]
    fn accepts_each_valid_prefix() {
        for prefix in VALID_PREFIXES {
            let msg = format!("{prefix}: do the thing");
            assert!(
                is_conventional(&msg),
                "VALID_PREFIXES contains {prefix:?} but is_conventional rejected {msg:?}",
            );
        }
    }

    #[test]
    fn accepts_scoped_prefix() {
        assert!(is_conventional("feat(api): add endpoint"));
        assert!(is_conventional("fix(parser): handle empty input"));
    }

    #[test]
    fn accepts_multiline_with_valid_subject() {
        let msg = "feat: add login\n\nLonger body explaining motivation.\n\nFooter";
        assert!(is_conventional(msg));
    }

    #[test]
    fn case_insensitive_prefix() {
        // The original implementation lowercased before comparing.
        assert!(is_conventional("FEAT: shout"));
        assert!(is_conventional("Fix: mixed case"));
    }

    #[test]
    fn rejects_empty() {
        assert!(!is_conventional(""));
        assert!(!is_conventional("   "));
        assert!(!is_conventional("\n\n\n"));
    }

    #[test]
    fn rejects_unknown_prefix() {
        assert!(!is_conventional("nope: typo"));
        assert!(!is_conventional("update: not in list"));
    }

    #[test]
    fn rejects_missing_colon() {
        assert!(!is_conventional("feat add login"));
    }

    #[test]
    fn rejects_missing_description() {
        // The regex requires `: <description>` with at least one non-space
        // char after the colon.
        assert!(!is_conventional("feat:"));
        assert!(!is_conventional("feat: "));
    }

    // ─── has_linear_ref ────────────────────────────────────────────────

    #[test]
    fn detects_simple_ref() {
        let prefixes = vec!["FPCRM".to_string()];
        assert!(has_linear_ref("Ref FPCRM-42", &prefixes));
    }

    #[test]
    fn detects_ref_anywhere_in_message() {
        let prefixes = vec!["FPCRM".to_string()];
        assert!(has_linear_ref(
            "fix(api): correct query — see FPCRM-42 for context",
            &prefixes,
        ));
    }

    #[test]
    fn case_insensitive_match() {
        let prefixes = vec!["FPCRM".to_string()];
        assert!(has_linear_ref("ref fpcrm-42", &prefixes));
        assert!(has_linear_ref("Ref FpCrM-42", &prefixes));
    }

    #[test]
    fn requires_word_boundary() {
        let prefixes = vec!["FPCRM".to_string()];
        // Hyphenated mid-word should not match — boundary anchors on both sides.
        assert!(!has_linear_ref("xFPCRM-42", &prefixes));
        // Trailing letters after the digits also not a match.
        assert!(!has_linear_ref("FPCRM-42xyz", &prefixes));
    }

    #[test]
    fn requires_numeric_tail() {
        let prefixes = vec!["FPCRM".to_string()];
        assert!(!has_linear_ref("FPCRM-foo", &prefixes));
        assert!(!has_linear_ref("FPCRM-", &prefixes));
        assert!(!has_linear_ref("FPCRM", &prefixes));
    }

    #[test]
    fn matches_any_of_multiple_prefixes() {
        let prefixes = vec!["FPCRM".to_string(), "FPFIELD".to_string()];
        assert!(has_linear_ref("Ref FPFIELD-7", &prefixes));
        assert!(has_linear_ref("Ref FPCRM-1", &prefixes));
        assert!(!has_linear_ref("Ref UNKNOWN-1", &prefixes));
    }

    #[test]
    fn empty_prefix_list_returns_false() {
        assert!(!has_linear_ref("FPCRM-42", &[]));
    }

    #[test]
    fn regex_special_chars_in_prefix_are_escaped() {
        // Pathological: prefix containing a regex meta-char shouldn't blow
        // up via injection — `regex::escape` neutralizes it.
        let prefixes = vec!["A.B".to_string()];
        // Only the literal `A.B-42` matches; `AxB-42` should not.
        assert!(has_linear_ref("Ref A.B-42", &prefixes));
        assert!(!has_linear_ref("Ref AxB-42", &prefixes));
    }
}
