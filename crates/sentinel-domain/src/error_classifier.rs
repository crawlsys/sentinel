//! Error-classification domain predicates.
//!
//! Pure rules for deciding which entries in the sentinel error log deserve
//! to be surfaced into the next user prompt. Extracted from the
//! `error_reporter` hook so the rule is reviewable + testable in isolation
//! and can evolve without changing the hook's orchestration code.
//!
//! The hook is still responsible for **well-formedness** of an entry
//! (do the required fields exist?). This module answers a separate
//! question: given a well-formed entry, is its `error` *category* worth
//! reporting?

/// Error categories that are intentionally NOT surfaced into prompts:
///
/// - `rate_limit` — transient, the user already sees the system message.
/// - `auth_error` — credentials issue, action belongs in setup not prompts.
/// - `invalid_request` — usually a transient bad call we already retried.
///
/// These are common runtime / session conditions, not durable infrastructure
/// defects worth nagging about. Keep this list **small** — every addition
/// silences a real failure mode, so the bias is toward *reporting*, not
/// suppressing.
pub const NON_ACTIONABLE_ERROR_CATEGORIES: &[&str] =
    &["rate_limit", "auth_error", "invalid_request"];

/// Substrings that indicate a non-actionable error even when the category itself doesn't match.
///
/// The current entry was added because the `prompt is too long` failures from
/// Claude Code's compactor were producing recurring noise.
pub const NON_ACTIONABLE_ERROR_SUBSTRINGS: &[&str] = &["prompt is too long"];

/// Decide whether an error log entry's category is worth surfacing into
/// the next user prompt.
///
/// Returns `false` if the error matches any value in
/// [`NON_ACTIONABLE_ERROR_CATEGORIES`] (exact match) OR contains any value
/// in [`NON_ACTIONABLE_ERROR_SUBSTRINGS`].
///
/// **Not checked here**: well-formedness (non-empty id/component/etc.) —
/// that is the caller's responsibility because it depends on how the entry
/// was deserialised, which the domain shouldn't know about.
#[must_use]
pub fn is_actionable_error(error: &str) -> bool {
    if NON_ACTIONABLE_ERROR_CATEGORIES.contains(&error) {
        return false;
    }
    if NON_ACTIONABLE_ERROR_SUBSTRINGS
        .iter()
        .any(|sub| error.contains(sub))
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_each_categorical_non_actionable() {
        for cat in NON_ACTIONABLE_ERROR_CATEGORIES {
            assert!(
                !is_actionable_error(cat),
                "expected {cat:?} to be non-actionable",
            );
        }
    }

    #[test]
    fn rejects_substring_non_actionable() {
        // The compactor failure has surrounding text but contains the marker.
        assert!(!is_actionable_error(
            "compactor: prompt is too long for the model"
        ));
        assert!(!is_actionable_error("prompt is too long"));
    }

    #[test]
    fn accepts_arbitrary_actionable_categories() {
        assert!(is_actionable_error("file_not_found"));
        assert!(is_actionable_error("internal_error"));
        assert!(is_actionable_error("disk_full"));
    }

    #[test]
    fn category_check_is_exact_match_not_substring() {
        // `rate_limit` is non-actionable, but `rate_limited_v2` is a different
        // category that should pass through. The categorical check is `==`,
        // not `contains`.
        assert!(is_actionable_error("rate_limited_v2"));
        // And a substring containing a non-actionable category in the middle
        // also doesn't get suppressed by the categorical check (it would only
        // be suppressed by the substring list, which is intentionally tiny).
        assert!(is_actionable_error("upstream_rate_limit_recovery"));
    }

    #[test]
    fn empty_string_is_actionable() {
        // Defensive: empty string isn't in either list. The hook's
        // well-formedness check handles this case before calling here.
        assert!(is_actionable_error(""));
    }
}
