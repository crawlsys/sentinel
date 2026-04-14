//! Session identity types.
//!
//! Strongly-typed wrappers for session-related identifiers.
//! Introduced as part of the DDD type-safety audit (P3).

use serde::{Deserialize, Serialize};

/// Strongly-typed wrapper for Claude Code session identifiers.
///
/// Replaces raw `String` usage throughout the codebase to prevent
/// accidental misuse (e.g., passing a skill name where a session ID
/// is expected). Migration of all call sites is tracked separately.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Create a new `SessionId` from any string-like value.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_and_as_str() {
        let id = SessionId::new("sess-abc-123");
        assert_eq!(id.as_str(), "sess-abc-123");
    }

    #[test]
    fn test_display() {
        let id = SessionId::new("sess-display");
        assert_eq!(format!("{id}"), "sess-display");
    }

    #[test]
    fn test_from_string() {
        let id: SessionId = String::from("sess-from-string").into();
        assert_eq!(id.as_str(), "sess-from-string");
    }

    #[test]
    fn test_from_str() {
        let id: SessionId = "sess-from-str".into();
        assert_eq!(id.as_str(), "sess-from-str");
    }

    #[test]
    fn test_equality() {
        let a = SessionId::new("same");
        let b = SessionId::new("same");
        let c = SessionId::new("different");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_hash_key() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(SessionId::new("key"), 42);
        assert_eq!(map.get(&SessionId::new("key")), Some(&42));
    }

    #[test]
    fn test_clone() {
        let original = SessionId::new("clone-me");
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_serde_roundtrip() {
        let id = SessionId::new("sess-serde");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }
}
