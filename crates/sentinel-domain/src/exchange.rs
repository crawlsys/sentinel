//! Conversation-exchange domain predicates.
//!
//! Rules for deciding whether a (user, assistant) message pair is
//! substantive enough to index into the memory engine. Extracted from
//! `memory_extract` so the rule is reviewable and testable in isolation.

use crate::constants::MIN_EXCHANGE_LENGTH;

/// User-side phrases that, taken alone, indicate a trivial exchange.
///
/// When the user message exactly matches (case-insensitive, after trim) one of
/// these AND the assistant reply is short, the exchange is dropped.
///
/// The list is intentionally short; expanding it risks dropping real
/// exchanges that happen to start with one of these tokens.
pub const TRIVIAL_USER_PHRASES: &[&str] = &[
    "yes",
    "no",
    "ok",
    "okay",
    "done",
    "thanks",
    "thank you",
    "got it",
    "sure",
    "y",
    "n",
    "yep",
    "nope",
    "continue",
    "go",
    "next",
    "fix it",
    "all",
    "yee",
    "cool",
    "nice",
    "great",
    "perfect",
];

/// Maximum assistant-reply length (chars) for which a trivial user phrase suppresses the exchange.
///
/// Longer assistant replies override the triviality heuristic — if the assistant
/// wrote a paragraph in response to "yes", that paragraph is probably substantive context.
pub const TRIVIAL_REPLY_MAX_LEN: usize = 200;

/// Return `true` if `(user, assistant)` is substantive enough to index.
///
/// Rules:
/// 1. `user.len() + assistant.len()` must be ≥ [`MIN_EXCHANGE_LENGTH`].
/// 2. If the trimmed, lower-cased user message exactly matches a phrase
///    in [`TRIVIAL_USER_PHRASES`] AND `assistant.len() < TRIVIAL_REPLY_MAX_LEN`,
///    the exchange is non-substantive even if rule 1 passes.
///
/// The rule operates on the surface text only; semantic substance (e.g.
/// "this is the password" might be a 20-char exchange that's *very*
/// substantive) is the LLM's job, not this filter.
#[must_use]
pub fn is_substantive_exchange(user: &str, assistant: &str) -> bool {
    if user.len() + assistant.len() < MIN_EXCHANGE_LENGTH {
        return false;
    }
    let user_trimmed = user.trim().to_lowercase();
    if TRIVIAL_USER_PHRASES.contains(&user_trimmed.as_str())
        && assistant.len() < TRIVIAL_REPLY_MAX_LEN
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_text(n: usize) -> String {
        "x".repeat(n)
    }

    #[test]
    fn drops_too_short_combined() {
        // Below MIN_EXCHANGE_LENGTH (100) → not substantive.
        assert!(!is_substantive_exchange("hi", "hello"));
    }

    #[test]
    fn accepts_long_combined_with_non_trivial_user() {
        let user = "Walk me through how the proof chain works.";
        let assistant = long_text(150);
        assert!(is_substantive_exchange(user, &assistant));
    }

    #[test]
    fn drops_trivial_user_with_short_reply() {
        // User says "yes", assistant replies briefly → not substantive.
        let user = "yes";
        let assistant = long_text(150); // > MIN_EXCHANGE_LENGTH but < TRIVIAL_REPLY_MAX_LEN
        assert!(!is_substantive_exchange(user, &assistant));
    }

    #[test]
    fn keeps_trivial_user_with_long_reply() {
        // User says "yes" but the assistant elaborates — keep it.
        let user = "yes";
        let assistant = long_text(TRIVIAL_REPLY_MAX_LEN + 1);
        assert!(is_substantive_exchange(user, &assistant));
    }

    #[test]
    fn trivial_match_is_case_insensitive_and_trimmed() {
        // "  YES  " trims and lower-cases to "yes" → trivial.
        let user = "  YES  ";
        let assistant = long_text(150);
        assert!(!is_substantive_exchange(user, &assistant));
    }

    #[test]
    fn trivial_match_is_exact_not_substring() {
        // "yes please" does NOT match the trivial set (only exact "yes" does).
        let user = "yes please walk me through this in detail";
        let assistant = long_text(150);
        assert!(is_substantive_exchange(user, &assistant));
    }

    #[test]
    fn each_trivial_phrase_is_recognized() {
        for phrase in TRIVIAL_USER_PHRASES {
            let assistant = long_text(150);
            assert!(
                !is_substantive_exchange(phrase, &assistant),
                "expected {phrase:?} + short reply to be non-substantive",
            );
        }
    }
}
