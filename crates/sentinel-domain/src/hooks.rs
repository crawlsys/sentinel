//! Hook definitions and traits
//!
//! Each hook has an ID, event type, optional matcher, and dependency list.

use serde::{Deserialize, Serialize};

use crate::events::HookEvent;

/// Unique identifier for a hook
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HookId(String);

impl HookId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for HookId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Hook specification loaded from config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    /// Unique hook identifier
    pub id: HookId,

    /// Which lifecycle event this hook fires on
    pub event: HookEvent,

    /// Tool matchers (empty = fires on all tools for this event)
    #[serde(default)]
    pub matcher: Vec<String>,

    /// Hook IDs that must complete before this hook runs
    #[serde(default)]
    pub depends_on: Vec<HookId>,

    /// Whether this hook makes API calls (used for scheduling)
    #[serde(default)]
    pub has_api_call: bool,
}

impl HookSpec {
    /// Check if this hook should fire for a given event and optional tool matcher
    #[must_use]
    pub fn matches(&self, event: HookEvent, tool_matcher: Option<&str>) -> bool {
        if self.event != event {
            return false;
        }

        // If hook has no matchers, it fires for all tools
        if self.matcher.is_empty() {
            return true;
        }

        // If a tool matcher is provided, check if it's in our list
        match tool_matcher {
            Some(tool) => self.matcher.iter().any(|m| m == tool),
            None => true, // No tool context = match everything
        }
    }
}

/// Result of executing a single hook
#[derive(Debug, Clone)]
pub struct HookResult {
    /// Which hook produced this result
    pub hook_id: HookId,

    /// Execution time in milliseconds
    pub duration_ms: u64,

    /// The output to merge into the final response
    pub output: crate::events::HookOutput,

    /// Whether execution succeeded
    pub success: bool,

    /// Error message if execution failed
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(id: &str, event: HookEvent, matchers: Vec<&str>) -> HookSpec {
        HookSpec {
            id: HookId::new(id),
            event,
            matcher: matchers.into_iter().map(String::from).collect(),
            depends_on: vec![],
            has_api_call: false,
        }
    }

    #[test]
    fn test_matches_event_only() {
        let spec = make_spec("test", HookEvent::Stop, vec![]);
        assert!(spec.matches(HookEvent::Stop, None));
        assert!(!spec.matches(HookEvent::PreToolUse, None));
    }

    #[test]
    fn test_matches_with_tool_matcher() {
        let spec = make_spec("test", HookEvent::PreToolUse, vec!["Bash", "Edit"]);
        assert!(spec.matches(HookEvent::PreToolUse, Some("Bash")));
        assert!(spec.matches(HookEvent::PreToolUse, Some("Edit")));
        assert!(!spec.matches(HookEvent::PreToolUse, Some("Write")));
    }

    #[test]
    fn test_no_matcher_matches_all_tools() {
        let spec = make_spec("test", HookEvent::PreToolUse, vec![]);
        assert!(spec.matches(HookEvent::PreToolUse, Some("Bash")));
        assert!(spec.matches(HookEvent::PreToolUse, Some("anything")));
    }
}
