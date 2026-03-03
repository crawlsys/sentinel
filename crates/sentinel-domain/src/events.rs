//! Hook lifecycle events
//!
//! Maps to Claude Code's 6 hook lifecycle events.

use serde::{Deserialize, Serialize};

/// Claude Code hook lifecycle events
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    PreCompact,
}

impl HookEvent {
    /// Parse from CLI argument string
    #[must_use]
    pub fn from_arg(s: &str) -> Option<Self> {
        match s {
            "SessionStart" => Some(Self::SessionStart),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "PreToolUse" => Some(Self::PreToolUse),
            "PostToolUse" => Some(Self::PostToolUse),
            "Stop" => Some(Self::Stop),
            "PreCompact" => Some(Self::PreCompact),
            _ => None,
        }
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionStart => write!(f, "SessionStart"),
            Self::UserPromptSubmit => write!(f, "UserPromptSubmit"),
            Self::PreToolUse => write!(f, "PreToolUse"),
            Self::PostToolUse => write!(f, "PostToolUse"),
            Self::Stop => write!(f, "Stop"),
            Self::PreCompact => write!(f, "PreCompact"),
        }
    }
}

/// Raw JSON input from Claude Code's hook system
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookInput {
    /// The hook event type (not always present in stdin, inferred from CLI args)
    #[serde(default)]
    pub hook_event: Option<String>,

    /// Session ID
    #[serde(default)]
    pub session_id: Option<String>,

    /// User's prompt text (UserPromptSubmit)
    #[serde(default)]
    pub prompt: Option<String>,

    /// Current working directory
    #[serde(default)]
    pub cwd: Option<String>,

    /// Tool name being called (PreToolUse/PostToolUse)
    #[serde(default)]
    pub tool_name: Option<String>,

    /// Tool input arguments (PreToolUse/PostToolUse)
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,

    /// Tool result (PostToolUse)
    #[serde(default)]
    pub tool_result: Option<serde_json::Value>,

    /// Permission mode
    #[serde(default)]
    pub permission_mode: Option<String>,

    /// Transcript path
    #[serde(default)]
    pub transcript_path: Option<String>,

    /// Context window info (Stop)
    #[serde(default)]
    pub context_window: Option<serde_json::Value>,

    /// Catch-all for unknown fields
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// JSON output a hook returns to Claude Code
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookOutput {
    /// If true, the tool call is blocked (PreToolUse only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,

    /// Reason for blocking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Hook-specific output (UserPromptSubmit context injection)
    #[serde(skip_serializing_if = "Option::is_none", rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
}

impl HookOutput {
    /// Empty response — allow everything
    #[must_use]
    pub fn allow() -> Self {
        Self::default()
    }

    /// Block a tool call with a reason
    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            blocked: Some(true),
            reason: Some(reason.into()),
            hook_specific_output: None,
        }
    }

    /// Inject additional context into the conversation
    #[must_use]
    pub fn inject_context(event: HookEvent, context: impl Into<String>) -> Self {
        Self {
            blocked: None,
            reason: None,
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: event.to_string(),
                additional_context: context.into(),
            }),
        }
    }

    /// Merge another output into this one. Blocked wins over allowed.
    pub fn merge(&mut self, other: &Self) {
        // If either blocks, the merged result blocks
        if other.blocked == Some(true) {
            self.blocked = Some(true);
            if let Some(ref reason) = other.reason {
                let existing = self.reason.take().unwrap_or_default();
                self.reason = Some(if existing.is_empty() {
                    reason.clone()
                } else {
                    format!("{existing}\n\n{reason}")
                });
            }
        }

        // Merge context injections
        if let Some(ref other_ctx) = other.hook_specific_output {
            if let Some(ref mut self_ctx) = self.hook_specific_output {
                self_ctx.additional_context = format!(
                    "{}\n\n{}",
                    self_ctx.additional_context, other_ctx.additional_context
                );
            } else {
                self.hook_specific_output = Some(other_ctx.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_from_arg() {
        assert_eq!(HookEvent::from_arg("PreToolUse"), Some(HookEvent::PreToolUse));
        assert_eq!(HookEvent::from_arg("Stop"), Some(HookEvent::Stop));
        assert_eq!(HookEvent::from_arg("invalid"), None);
    }

    #[test]
    fn test_hook_output_allow() {
        let output = HookOutput::allow();
        let json = serde_json::to_string(&output).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn test_hook_output_block() {
        let output = HookOutput::block("tests not run");
        assert_eq!(output.blocked, Some(true));
        assert_eq!(output.reason.as_deref(), Some("tests not run"));
    }

    #[test]
    fn test_hook_output_merge_block_wins() {
        let mut a = HookOutput::allow();
        let b = HookOutput::block("blocked by gate");
        a.merge(&b);
        assert_eq!(a.blocked, Some(true));
    }

    #[test]
    fn test_hook_output_merge_contexts() {
        let mut a = HookOutput::inject_context(HookEvent::UserPromptSubmit, "context A");
        let b = HookOutput::inject_context(HookEvent::UserPromptSubmit, "context B");
        a.merge(&b);
        let ctx = a.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("context A"));
        assert!(ctx.contains("context B"));
    }
}
