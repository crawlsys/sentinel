//! Session identity types.
//!
//! Strongly-typed wrappers for session-related identifiers with validation
//! at the type-system boundary. The validating smart constructor centralizes
//! the formerly-duplicated `sanitize_session_id` logic that lived in both
//! `state_store` and `proof_store` (Attack #121 — path traversal via
//! malicious `session_id`).

use serde::{Deserialize, Serialize};

/// Maximum length of a session identifier in characters. Prevents pathological
/// long-string `DoS` in path-handling code.
pub const SESSION_ID_MAX_LEN: usize = 128;

/// Reasons a string fails to qualify as a `SessionId`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIdError {
    /// Session ID was the empty string.
    Empty,
    /// Session ID exceeded `SESSION_ID_MAX_LEN`.
    TooLong { len: usize },
    /// Session ID contained `..` — path traversal attempt.
    PathTraversal,
    /// Session ID contained a character outside `[a-zA-Z0-9_-]`.
    UnsafeCharacter,
}

impl std::fmt::Display for SessionIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "Session ID is empty"),
            Self::TooLong { len } => write!(
                f,
                "Session ID too long (max {SESSION_ID_MAX_LEN} chars): {len}"
            ),
            Self::PathTraversal => write!(f, "Session ID contains path traversal: '..'"),
            Self::UnsafeCharacter => write!(
                f,
                "Session ID contains unsafe characters (only ASCII alphanumeric, '-', '_' allowed)"
            ),
        }
    }
}

impl std::error::Error for SessionIdError {}

/// Strongly-typed wrapper for Claude Code session identifiers.
///
/// Any `SessionId` value has passed the validation rules required for safe
/// filesystem use:
///
/// - Non-empty
/// - At most `SESSION_ID_MAX_LEN` (128) characters
/// - Does not contain the path-traversal sequence `..`
/// - Only ASCII alphanumeric characters, `-`, and `_`
///
/// Construction:
/// - [`SessionId::try_new`] — validates and returns `Result<SessionId, SessionIdError>`
/// - [`SessionId::new_unchecked`] — bypasses validation; only safe when the
///   caller has *just* validated via `try_new` or is reconstituting from
///   trusted persistent state. New code should always prefer `try_new`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Validate and construct a `SessionId`. Returns `Err` if the input fails
    /// any rule (non-empty, length cap, no `..`, ASCII alphanumeric/-/_ only).
    pub fn try_new(id: impl Into<String>) -> Result<Self, SessionIdError> {
        let id = id.into();
        Self::validate(&id)?;
        Ok(Self(id))
    }

    /// Construct a `SessionId` without validation. Use only when the caller
    /// has just validated via `try_new` or is reconstituting from persistent
    /// state previously written by validated code paths.
    #[must_use]
    pub fn new_unchecked(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Validate a candidate session-id string against all rules without
    /// allocating a `SessionId`.
    pub fn validate(id: &str) -> Result<(), SessionIdError> {
        if id.is_empty() {
            return Err(SessionIdError::Empty);
        }
        if id.len() > SESSION_ID_MAX_LEN {
            return Err(SessionIdError::TooLong { len: id.len() });
        }
        if id.contains("..") {
            return Err(SessionIdError::PathTraversal);
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(SessionIdError::UnsafeCharacter);
        }
        Ok(())
    }

    /// Borrow the inner string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the `SessionId` and return the inner `String`.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_new_accepts_valid() {
        let id = SessionId::try_new("sess-abc-123").unwrap();
        assert_eq!(id.as_str(), "sess-abc-123");
    }

    #[test]
    fn try_new_rejects_empty() {
        assert_eq!(SessionId::try_new(""), Err(SessionIdError::Empty));
    }

    #[test]
    fn try_new_rejects_too_long() {
        let oversize = "a".repeat(SESSION_ID_MAX_LEN + 1);
        let len = oversize.len();
        assert_eq!(
            SessionId::try_new(oversize),
            Err(SessionIdError::TooLong { len })
        );
    }

    #[test]
    fn try_new_rejects_path_traversal() {
        // `..` is the path-traversal sentinel; must be rejected even when
        // the rest of the string is otherwise alphanumeric.
        assert_eq!(
            SessionId::try_new("sess..evil"),
            Err(SessionIdError::PathTraversal)
        );
    }

    #[test]
    fn try_new_rejects_path_traversal_full() {
        // Realistic Attack #121 payload — `../../etc/passwd`.
        // Rejected by either the path-traversal check or the unsafe-char
        // check; the order is well-defined (traversal check fires first).
        assert_eq!(
            SessionId::try_new("../../etc/passwd"),
            Err(SessionIdError::PathTraversal)
        );
    }

    #[test]
    fn try_new_rejects_unsafe_chars() {
        for bad in &["with space", "with/slash", "with.dot", "with!bang"] {
            assert_eq!(
                SessionId::try_new(*bad),
                Err(SessionIdError::UnsafeCharacter),
                "expected rejection for {bad:?}",
            );
        }
    }

    #[test]
    fn try_new_accepts_underscore_and_hyphen() {
        SessionId::try_new("a_b-c_d-1_2").unwrap();
    }

    #[test]
    fn validate_does_not_allocate() {
        // Tests the `&str`-only entry point for fast pre-checks.
        assert!(SessionId::validate("good-id").is_ok());
        assert_eq!(SessionId::validate(""), Err(SessionIdError::Empty));
    }

    #[test]
    fn new_unchecked_bypasses_validation() {
        // `new_unchecked` is the documented escape hatch for reconstituting
        // session IDs from already-validated persistent state. It MUST NOT
        // run validation, otherwise replaying a previously-valid record that
        // was loosened by a config change would silently break.
        let bypass = SessionId::new_unchecked("with..invalid");
        assert_eq!(bypass.as_str(), "with..invalid");
    }

    #[test]
    fn display_round_trips() {
        let id = SessionId::try_new("sess-display").unwrap();
        assert_eq!(format!("{id}"), "sess-display");
    }

    #[test]
    fn equality_and_hashing() {
        use std::collections::HashMap;
        let a = SessionId::try_new("same").unwrap();
        let b = SessionId::try_new("same").unwrap();
        let c = SessionId::try_new("different").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut map = HashMap::new();
        map.insert(a, 42);
        assert_eq!(map.get(&b), Some(&42));
    }

    #[test]
    fn serde_round_trip() {
        let id = SessionId::try_new("sess-serde").unwrap();
        let json = serde_json::to_string(&id).expect("serialize");
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn serde_deserializes_invalid_string_into_session_id() {
        // Documented gotcha: serde's `Deserialize` impl is *infallible*
        // (derived) and bypasses `try_new`. Persistent stores must
        // re-validate after deserializing if the source isn't trusted.
        // This test pins the current behaviour so future readers know.
        let invalid: SessionId =
            serde_json::from_str(r#""../etc/passwd""#).expect("derived deserialize");
        assert_eq!(invalid.as_str(), "../etc/passwd");
        // Re-validation catches it:
        assert!(SessionId::validate(invalid.as_str()).is_err());
    }
}
