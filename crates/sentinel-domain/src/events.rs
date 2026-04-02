//! Hook lifecycle events
//!
//! Maps to Claude Code's 27 hook lifecycle events (as of v2.1.88).
//! Sentinel handles 16 of these; remaining events are passed through.
//! Outputs conform to Claude Code's actual Zod-validated JSON schema
//! (discovered via source deobfuscation of CLI v2.1.50, updated v2.1.88).

use serde::{Deserialize, Serialize};

/// Claude Code hook lifecycle events
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    StopFailure,
    PreCompact,
    PostCompact,
    Setup,
    SubagentStart,
    SubagentStop,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    PermissionDenied,
    CwdChanged,
    PermissionRequest,
    Elicitation,
    ElicitationResult,
    ConfigChange,
    InstructionsLoaded,
    FileChanged,
    WorktreeCreate,
    WorktreeRemove,
    Notification,
}

/// PreToolUse permission decision — maps to Claude Code's permissionDecision field.
/// Priority: Deny > Ask > Allow (when merging multiple hook outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

impl HookEvent {
    /// Parse from CLI argument string
    #[must_use]
    pub fn from_arg(s: &str) -> Option<Self> {
        match s {
            "SessionStart" => Some(Self::SessionStart),
            "SessionEnd" => Some(Self::SessionEnd),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "PreToolUse" => Some(Self::PreToolUse),
            "PostToolUse" => Some(Self::PostToolUse),
            "PostToolUseFailure" => Some(Self::PostToolUseFailure),
            "Stop" => Some(Self::Stop),
            "StopFailure" => Some(Self::StopFailure),
            "PreCompact" => Some(Self::PreCompact),
            "PostCompact" => Some(Self::PostCompact),
            "Setup" => Some(Self::Setup),
            "SubagentStart" => Some(Self::SubagentStart),
            "SubagentStop" => Some(Self::SubagentStop),
            "TeammateIdle" => Some(Self::TeammateIdle),
            "TaskCreated" => Some(Self::TaskCreated),
            "TaskCompleted" => Some(Self::TaskCompleted),
            "PermissionDenied" => Some(Self::PermissionDenied),
            "CwdChanged" => Some(Self::CwdChanged),
            "PermissionRequest" => Some(Self::PermissionRequest),
            "Elicitation" => Some(Self::Elicitation),
            "ElicitationResult" => Some(Self::ElicitationResult),
            "ConfigChange" => Some(Self::ConfigChange),
            "InstructionsLoaded" => Some(Self::InstructionsLoaded),
            "FileChanged" => Some(Self::FileChanged),
            "WorktreeCreate" => Some(Self::WorktreeCreate),
            "WorktreeRemove" => Some(Self::WorktreeRemove),
            "Notification" => Some(Self::Notification),
            _ => None,
        }
    }
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionStart => write!(f, "SessionStart"),
            Self::SessionEnd => write!(f, "SessionEnd"),
            Self::UserPromptSubmit => write!(f, "UserPromptSubmit"),
            Self::PreToolUse => write!(f, "PreToolUse"),
            Self::PostToolUse => write!(f, "PostToolUse"),
            Self::PostToolUseFailure => write!(f, "PostToolUseFailure"),
            Self::Stop => write!(f, "Stop"),
            Self::StopFailure => write!(f, "StopFailure"),
            Self::PreCompact => write!(f, "PreCompact"),
            Self::PostCompact => write!(f, "PostCompact"),
            Self::Setup => write!(f, "Setup"),
            Self::SubagentStart => write!(f, "SubagentStart"),
            Self::SubagentStop => write!(f, "SubagentStop"),
            Self::TeammateIdle => write!(f, "TeammateIdle"),
            Self::TaskCreated => write!(f, "TaskCreated"),
            Self::TaskCompleted => write!(f, "TaskCompleted"),
            Self::PermissionDenied => write!(f, "PermissionDenied"),
            Self::CwdChanged => write!(f, "CwdChanged"),
            Self::PermissionRequest => write!(f, "PermissionRequest"),
            Self::Elicitation => write!(f, "Elicitation"),
            Self::ElicitationResult => write!(f, "ElicitationResult"),
            Self::ConfigChange => write!(f, "ConfigChange"),
            Self::InstructionsLoaded => write!(f, "InstructionsLoaded"),
            Self::FileChanged => write!(f, "FileChanged"),
            Self::WorktreeCreate => write!(f, "WorktreeCreate"),
            Self::WorktreeRemove => write!(f, "WorktreeRemove"),
            Self::Notification => write!(f, "Notification"),
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

    /// Last assistant message text (Stop/SubagentStop)
    #[serde(default)]
    pub last_assistant_message: Option<String>,

    /// Agent transcript path (SubagentStop)
    #[serde(default)]
    pub agent_transcript_path: Option<String>,

    /// Compact summary text (PostCompact)
    #[serde(default)]
    pub compact_summary: Option<String>,

    /// Permission suggestions (PermissionRequest)
    #[serde(default)]
    pub permission_suggestions: Option<Vec<serde_json::Value>>,

    /// Catch-all for unknown fields — absorbs new Claude Code fields without
    /// breaking deserialization. **Attack #124 note**: Values here are untrusted
    /// and unvalidated. Hooks that read from `extra` MUST treat values as
    /// potentially attacker-controlled. Never use `extra` for security-critical
    /// decisions (tool gating, skill routing, phase advancement).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// JSON output a hook returns to Claude Code
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookOutput {
    /// If true, the tool call is blocked (internal merge flag, cleared on output)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,

    /// Reason for blocking (internal, cleared on output for PreToolUse)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Hook-specific output — the primary output mechanism for Claude Code
    #[serde(skip_serializing_if = "Option::is_none", rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,

    /// Warning message shown to the user in the terminal (visible in transcript).
    /// Used for banners and notifications that the user should see directly.
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemMessage")]
    pub system_message: Option<String>,

    /// If false, prevents the model from continuing (used by Stop hooks)
    #[serde(skip_serializing_if = "Option::is_none", rename = "continue")]
    pub continue_: Option<bool>,

    /// Suppress hook output display
    #[serde(skip_serializing_if = "Option::is_none", rename = "suppressOutput")]
    pub suppress_output: Option<bool>,

    /// Custom reason when preventing continuation
    #[serde(skip_serializing_if = "Option::is_none", rename = "stopReason")]
    pub stop_reason: Option<String>,

    /// Simplified permission shorthand --- "approve" or "block"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
}

/// Claude Code's hookSpecificOutput schema (v2.1.88).
/// For PreToolUse: permissionDecision, permissionDecisionReason, updatedInput, additionalContext
/// For UserPromptSubmit/PostToolUse/SubagentStart/Setup: additionalContext
/// For SessionStart: additionalContext, initialUserMessage, watchPaths
/// For PermissionDenied: retry
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    /// Permission decision for PreToolUse (deny > ask > allow)
    #[serde(skip_serializing_if = "Option::is_none", rename = "permissionDecision")]
    pub permission_decision: Option<PermissionDecision>,

    /// String injected as if the user typed it (SessionStart only)
    #[serde(skip_serializing_if = "Option::is_none", rename = "initialUserMessage")]
    pub initial_user_message: Option<String>,

    /// File paths to monitor for FileChanged events (SessionStart only)
    #[serde(skip_serializing_if = "Option::is_none", rename = "watchPaths")]
    pub watch_paths: Option<Vec<String>>,

    /// Reason for the permission decision
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permissionDecisionReason"
    )]
    pub permission_decision_reason: Option<String>,

    /// Modified tool input (PreToolUse only — replaces the original input)
    #[serde(skip_serializing_if = "Option::is_none", rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,

    /// Additional context injected into the conversation
    #[serde(skip_serializing_if = "Option::is_none", rename = "additionalContext")]
    pub additional_context: Option<String>,

    /// Modified MCP tool output (PostToolUse only — replaces the original result)
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "updatedMCPToolOutput"
    )]
    pub updated_mcp_tool_output: Option<serde_json::Value>,

    /// Allow the model to retry a denied tool call (PermissionDenied only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,

    /// Elicitation/ElicitationResult action ("accept"/"decline"/"cancel")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,

    /// Elicitation/ElicitationResult content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,

    /// PermissionRequest decision
    #[serde(skip_serializing_if = "Option::is_none", rename = "permissionRequestDecision")]
    pub permission_request_decision: Option<serde_json::Value>,

    /// WorktreeCreate output path
    #[serde(skip_serializing_if = "Option::is_none", rename = "worktreePath")]
    pub worktree_path: Option<String>,
}

impl HookOutput {
    /// Empty response — allow everything
    #[must_use]
    pub fn allow() -> Self {
        Self::default()
    }

    /// Block a tool call with a reason (legacy — sets internal blocked flag).
    /// For PreToolUse, this is transformed to permissionDecision: deny on output.
    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            blocked: Some(true),
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Hard-deny a PreToolUse tool call (platform-enforced block).
    /// Uses Claude Code's hookSpecificOutput.permissionDecision directly.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            blocked: Some(true), // keep for internal merge logic
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: Some(PermissionDecision::Deny),
                permission_decision_reason: Some(reason.into()),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Prompt user for approval before allowing tool call (PreToolUse only)
    #[must_use]
    pub fn ask(reason: impl Into<String>) -> Self {
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: Some(PermissionDecision::Ask),
                permission_decision_reason: Some(reason.into()),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Modify tool input before execution (PreToolUse only)
    #[must_use]
    pub fn rewrite_input(updated: serde_json::Value) -> Self {
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: Some(PermissionDecision::Allow),
                updated_input: Some(updated),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Inject additional context into the conversation
    #[must_use]
    pub fn inject_context(event: HookEvent, context: impl Into<String>) -> Self {
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: event.to_string(),
                additional_context: Some(context.into()),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Allow the model to retry a denied tool call (PermissionDenied only).
    #[must_use]
    pub fn retry_denied() -> Self {
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "PermissionDenied".to_string(),
                retry: Some(true),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Prevent the model from continuing (Stop hooks)
    #[must_use]
    pub fn stop_continuation(reason: impl Into<String>) -> Self {
        Self {
            continue_: Some(false),
            stop_reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Transform legacy blocked/reason into proper Claude Code PreToolUse JSON.
    /// Called at the output boundary in hook_cmd.rs before serialization.
    #[must_use]
    pub fn into_pretool_output(mut self) -> Self {
        // If already has hookSpecificOutput with permissionDecision, clear legacy fields
        if self
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision)
            .is_some()
        {
            self.blocked = None;
            self.reason = None;
            return self;
        }

        // Transform legacy blocked/reason → hookSpecificOutput deny
        if self.blocked == Some(true) {
            let reason = self.reason.take();
            let existing_context = self
                .hook_specific_output
                .as_ref()
                .and_then(|h| h.additional_context.clone());
            self.blocked = None;
            self.hook_specific_output = Some(HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: Some(PermissionDecision::Deny),
                permission_decision_reason: reason,
                additional_context: existing_context,
                ..HookSpecificOutput::default()
            });
        }

        self
    }

    /// Merge another output into this one. Blocked wins over allowed.
    /// Permission decision priority: deny > ask > allow.
    pub fn merge(&mut self, other: &Self) {
        // Legacy blocked field merge (internal — transformed on output)
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

        // Merge hookSpecificOutput
        if let Some(ref other_hso) = other.hook_specific_output {
            match &mut self.hook_specific_output {
                Some(ref mut self_hso) => {
                    // Permission decision priority: deny > ask > allow
                    if let Some(other_pd) = other_hso.permission_decision {
                        let dominated = match (self_hso.permission_decision, other_pd) {
                            (_, PermissionDecision::Deny) => true,
                            (Some(PermissionDecision::Deny), _) => false,
                            (_, PermissionDecision::Ask) => true,
                            (Some(PermissionDecision::Ask), _) => false,
                            _ => true,
                        };
                        if dominated {
                            self_hso.permission_decision = Some(other_pd);
                            self_hso.permission_decision_reason =
                                other_hso.permission_decision_reason.clone();
                        }
                    }

                    // updatedInput: last writer wins
                    // **Attack #149 fix**: Clear updatedInput when permission is Deny.
                    // A contradictory Deny + updatedInput could confuse clients into
                    // executing a rewritten tool call despite the deny decision.
                    if self_hso.permission_decision == Some(PermissionDecision::Deny) {
                        self_hso.updated_input = None;
                    } else if other_hso.updated_input.is_some() {
                        self_hso.updated_input = other_hso.updated_input.clone();
                    }

                    // additionalContext: concatenate
                    // **Attack #136 fix**: Cap total additionalContext to 32KB.
                    // Without a limit, merged hook outputs could grow unbounded.
                    // 32KB is generous for all legitimate hook context combined.
                    const MAX_CONTEXT_LEN: usize = 32_768;
                    match (&self_hso.additional_context, &other_hso.additional_context) {
                        (Some(a), Some(b)) => {
                            let merged = format!("{a}\n\n{b}");
                            if merged.len() > MAX_CONTEXT_LEN {
                                self_hso.additional_context =
                                    Some(merged[..MAX_CONTEXT_LEN].to_string());
                            } else {
                                self_hso.additional_context = Some(merged);
                            }
                        }
                        (None, Some(b)) => {
                            if b.len() > MAX_CONTEXT_LEN {
                                self_hso.additional_context =
                                    Some(b[..MAX_CONTEXT_LEN].to_string());
                            } else {
                                self_hso.additional_context = Some(b.clone());
                            }
                        }
                        _ => {}
                    }
                }
                None => {
                    self.hook_specific_output = Some(other_hso.clone());
                }
            }
        }

        // Merge systemMessage: concatenate with newline
        // **Attack #102 fix**: Cap total systemMessage length to 4KB.
        // Without a limit, a compromised hook could inject megabytes of text
        // into the system message, either causing DoS or burying legitimate
        // warnings in noise. 4KB is generous for real warnings.
        match (&self.system_message, &other.system_message) {
            (Some(a), Some(b)) => {
                let merged = format!("{a}\n{b}");
                if merged.len() > 4096 {
                    self.system_message = Some(merged[..4096].to_string());
                } else {
                    self.system_message = Some(merged);
                }
            }
            (None, Some(b)) => {
                if b.len() > 4096 {
                    self.system_message = Some(b[..4096].to_string());
                } else {
                    self.system_message = Some(b.clone());
                }
            }
            _ => {}
        }

        // Merge continue_: false wins over true/None
        match (self.continue_, other.continue_) {
            (_, Some(false)) => self.continue_ = Some(false),
            (None, Some(true)) => self.continue_ = Some(true),
            _ => {}
        }

        // Merge suppress_output: true wins
        if other.suppress_output == Some(true) {
            self.suppress_output = Some(true);
        }

        // Merge stop_reason: append
        if let Some(ref reason) = other.stop_reason {
            match &self.stop_reason {
                Some(existing) => {
                    self.stop_reason = Some(format!("{existing}; {reason}"));
                }
                None => {
                    self.stop_reason = Some(reason.clone());
                }
            }
        }

        // Merge decision: "block" wins over "approve"
        match (&self.decision, &other.decision) {
            (_, Some(d)) if d == "block" => self.decision = Some("block".to_string()),
            (None, Some(d)) => self.decision = Some(d.clone()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_from_arg() {
        assert_eq!(
            HookEvent::from_arg("PreToolUse"),
            Some(HookEvent::PreToolUse)
        );
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
    fn test_hook_output_deny() {
        let output = HookOutput::deny("not allowed");
        assert_eq!(output.blocked, Some(true));
        let hso = output.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Deny));
        assert_eq!(
            hso.permission_decision_reason.as_deref(),
            Some("not allowed")
        );
    }

    #[test]
    fn test_hook_output_ask() {
        let output = HookOutput::ask("confirm deletion");
        assert!(output.blocked.is_none());
        let hso = output.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Ask));
    }

    #[test]
    fn test_hook_output_merge_block_wins() {
        let mut a = HookOutput::allow();
        let b = HookOutput::block("blocked by gate");
        a.merge(&b);
        assert_eq!(a.blocked, Some(true));
    }

    #[test]
    fn test_hook_output_merge_deny_over_ask() {
        let mut a = HookOutput::ask("maybe?");
        let b = HookOutput::deny("no way");
        a.merge(&b);
        let hso = a.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Deny));
        assert_eq!(hso.permission_decision_reason.as_deref(), Some("no way"));
    }

    #[test]
    fn test_hook_output_merge_ask_over_allow() {
        let mut a = HookOutput::allow();
        let b = HookOutput::ask("check this");
        a.merge(&b);
        let hso = a.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Ask));
    }

    #[test]
    fn test_hook_output_merge_deny_not_overridden_by_allow() {
        let mut a = HookOutput::deny("blocked");
        let b = HookOutput::allow();
        a.merge(&b);
        let hso = a.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Deny));
    }

    #[test]
    fn test_hook_output_merge_contexts() {
        let mut a = HookOutput::inject_context(HookEvent::UserPromptSubmit, "context A");
        let b = HookOutput::inject_context(HookEvent::UserPromptSubmit, "context B");
        a.merge(&b);
        let ctx = a.hook_specific_output.unwrap().additional_context.unwrap();
        assert!(ctx.contains("context A"));
        assert!(ctx.contains("context B"));
    }

    #[test]
    fn test_into_pretool_output_transforms_legacy_block() {
        let output = HookOutput::block("phase gate violation");
        let transformed = output.into_pretool_output();
        assert!(transformed.blocked.is_none());
        assert!(transformed.reason.is_none());
        let hso = transformed.hook_specific_output.unwrap();
        assert_eq!(hso.hook_event_name, "PreToolUse");
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Deny));
        assert_eq!(
            hso.permission_decision_reason.as_deref(),
            Some("phase gate violation")
        );
    }

    #[test]
    fn test_into_pretool_output_preserves_deny() {
        let output = HookOutput::deny("already proper");
        let transformed = output.into_pretool_output();
        assert!(transformed.blocked.is_none());
        let hso = transformed.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(PermissionDecision::Deny));
    }

    #[test]
    fn test_into_pretool_output_noop_for_allow() {
        let output = HookOutput::allow();
        let transformed = output.into_pretool_output();
        assert!(transformed.blocked.is_none());
        assert!(transformed.hook_specific_output.is_none());
    }
}
