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

/// `PreToolUse` permission decision — maps to Claude Code's permissionDecision field.
/// Priority: Deny > Ask > Defer > Allow (when merging multiple hook outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
    Defer,
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

    /// User's prompt text (`UserPromptSubmit`)
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

    /// Absolute file path for Write/Edit/Read tools (PreToolUse/PostToolUse, added 2.1.89)
    #[serde(default)]
    pub file_path: Option<String>,

    /// Tool result (`PostToolUse`).
    ///
    /// Claude Code sends this field as `tool_response` on the wire (verified in
    /// the deobfuscated 2.1.201 bundle: the PostToolUse payload and its zod
    /// schema both use `tool_response`; `tool_result` is never emitted). Without
    /// the alias every PostToolUse hook that inspects tool output — including the
    /// `prompt_injection_nudge` scanner — deserialized `None` and early-returned,
    /// silently disabling itself. The alias accepts both keys so the field
    /// populates from real CC payloads while existing `tool_result`-keyed test
    /// fixtures keep working.
    #[serde(default, alias = "tool_response")]
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

    /// Agent transcript path (`SubagentStop`)
    #[serde(default)]
    pub agent_transcript_path: Option<String>,

    /// Compact summary text (`PostCompact`)
    #[serde(default)]
    pub compact_summary: Option<String>,

    /// Permission suggestions (`PermissionRequest`)
    #[serde(default)]
    pub permission_suggestions: Option<Vec<serde_json::Value>>,

    /// Agent ID (present when hook fires from within a subagent)
    #[serde(default)]
    pub agent_id: Option<String>,

    /// Agent type name (e.g., "general-purpose", "code-reviewer")
    #[serde(default)]
    pub agent_type: Option<String>,

    /// Catch-all for unknown fields — absorbs new Claude Code fields without
    /// breaking deserialization. **Attack #124 note**: Values here are untrusted
    /// and unvalidated. Hooks that read from `extra` MUST treat values as
    /// potentially attacker-controlled. Never use `extra` for security-critical
    /// decisions (tool gating, skill routing, phase advancement).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Severity tier for `HookEnvelope` — drives the leading emoji in the
/// rendered prefix so users can scan hook output by urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookTier {
    /// Informational / observational — green dot.
    Info,
    /// Warning — user should notice but isn't blocked. Yellow dot.
    Warn,
    /// High-severity warning / soft block (dry-run auditors, BA structural
    /// gates). Orange dot — several hooks hand-rolled 🟠 because the enum
    /// couldn't express it.
    High,
    /// Blocking — the hook is rejecting the action. Red dot.
    Block,
}

impl HookTier {
    /// Single-glyph emoji prefix for this tier.
    #[must_use]
    pub const fn emoji(self) -> &'static str {
        match self {
            Self::Info => "🟢",
            Self::Warn => "🟡",
            Self::High => "🟠",
            Self::Block => "🔴",
        }
    }
}

/// Canonical envelope every hook should build its user-facing message with.
/// Renders to `[<name>] <emoji> <message>` so the surface stays consistent
/// across the 54 hooks that inject context.
///
/// `name` is the short hook identifier shown in brackets (e.g. `Skill Router`,
/// `Worktree Reminder`). Use Title Case with spaces, not the `snake_case` file
/// name — it's user-facing.
#[derive(Debug, Clone)]
pub struct HookEnvelope {
    pub name: String,
    pub tier: HookTier,
    pub message: String,
}

impl HookEnvelope {
    #[must_use]
    pub fn new(name: impl Into<String>, tier: HookTier, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tier,
            message: message.into(),
        }
    }

    /// Convenience: build an Info-tier envelope.
    #[must_use]
    pub fn info(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, HookTier::Info, message)
    }

    /// Convenience: build a Warn-tier envelope.
    #[must_use]
    pub fn warn(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, HookTier::Warn, message)
    }

    /// Convenience: build a Block-tier envelope.
    #[must_use]
    pub fn block(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(name, HookTier::Block, message)
    }

    /// Render to the canonical `[name] emoji message` string.
    #[must_use]
    pub fn render(&self) -> String {
        format!("[{}] {} {}", self.name, self.tier.emoji(), self.message)
    }
}

/// JSON output a hook returns to Claude Code
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookOutput {
    /// If true, the tool call is blocked (internal merge flag, cleared on output)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,

    /// Reason for blocking (internal, cleared on output for `PreToolUse`)
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
///
/// For `PreToolUse`: permissionDecision, permissionDecisionReason, updatedInput, additionalContext
/// For UserPromptSubmit/PostToolUse/SubagentStart/Setup: additionalContext
/// For `SessionStart`: additionalContext, initialUserMessage, watchPaths
/// For `PermissionDenied`: retry
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    /// Permission decision for `PreToolUse` (deny > ask > allow)
    #[serde(skip_serializing_if = "Option::is_none", rename = "permissionDecision")]
    pub permission_decision: Option<PermissionDecision>,

    /// String injected as if the user typed it (`SessionStart` only)
    #[serde(skip_serializing_if = "Option::is_none", rename = "initialUserMessage")]
    pub initial_user_message: Option<String>,

    /// File paths to monitor for `FileChanged` events (`SessionStart` only)
    #[serde(skip_serializing_if = "Option::is_none", rename = "watchPaths")]
    pub watch_paths: Option<Vec<String>>,

    /// Reason for the permission decision
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "permissionDecisionReason"
    )]
    pub permission_decision_reason: Option<String>,

    /// Modified tool input (`PreToolUse` only — replaces the original input)
    #[serde(skip_serializing_if = "Option::is_none", rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,

    /// Additional context injected into the conversation
    #[serde(skip_serializing_if = "Option::is_none", rename = "additionalContext")]
    pub additional_context: Option<String>,

    /// Modified MCP tool output (`PostToolUse` only — replaces the original result)
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "updatedMCPToolOutput"
    )]
    pub updated_mcp_tool_output: Option<serde_json::Value>,

    /// Allow the model to retry a denied tool call (`PermissionDenied` only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,

    /// Elicitation/ElicitationResult action ("accept"/"decline"/"cancel")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,

    /// Elicitation/ElicitationResult content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,

    /// `WorktreeCreate` output path
    #[serde(skip_serializing_if = "Option::is_none", rename = "worktreePath")]
    pub worktree_path: Option<String>,
}

/// Cap on merged `additionalContext` (32 KB). Prevents unbounded growth when
/// multiple hooks each inject context that gets concatenated in `HookOutput::merge`.
/// See Attack #136 fix in `merge()`.
const MAX_ADDITIONAL_CONTEXT_LEN: usize = 32_768;

impl HookOutput {
    /// Empty response — allow everything
    #[must_use]
    pub fn allow() -> Self {
        Self::default()
    }

    /// Block a tool call with a reason (legacy — sets internal blocked flag).
    /// For `PreToolUse`, this is transformed to permissionDecision: deny on output.
    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            blocked: Some(true),
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Provenance prefix tagged onto every sentinel-issued deny / ask
    /// reason. Sentinel is the only on-disk binary that constructs
    /// `HookOutput`, so this prefix appearing in tool-result text is
    /// proof that the directive came from sentinel — not from an
    /// arbitrary MCP server, web fetch, or model-injected string.
    ///
    /// Claude Code's CLAUDE.md "Hook Authority" section authorises the
    /// agent to auto-comply with directives carrying this prefix
    /// (including mode mutations like `EnterPlanMode`/`ExitPlanMode`),
    /// while treating untagged tool-result instructions as advisory.
    /// Without the prefix, the same coercive trick would let any tool
    /// result drive agent state — that is the failure mode we're
    /// closing off, not the sentinel-driven coercion itself.
    pub const SENTINEL_AUTHORITY_PREFIX: &'static str = "[Sentinel-Authority] ";

    /// Hard-deny a `PreToolUse` tool call (platform-enforced block).
    /// Uses Claude Code's hookSpecificOutput.permissionDecision directly.
    /// The reason is tagged with `[Sentinel-Authority]` so the agent can
    /// distinguish trusted sentinel directives from arbitrary tool-result
    /// text — see `SENTINEL_AUTHORITY_PREFIX`.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        let mut tagged = String::with_capacity(Self::SENTINEL_AUTHORITY_PREFIX.len() + 64);
        tagged.push_str(Self::SENTINEL_AUTHORITY_PREFIX);
        tagged.push_str(&reason.into());
        Self {
            blocked: Some(true), // keep for internal merge logic
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: Some(PermissionDecision::Deny),
                permission_decision_reason: Some(tagged),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Prompt user for approval before allowing tool call (`PreToolUse` only).
    /// Same authority tagging as `deny` — the prefix is what lets the agent
    /// trust the directive.
    #[must_use]
    pub fn ask(reason: impl Into<String>) -> Self {
        let mut tagged = String::with_capacity(Self::SENTINEL_AUTHORITY_PREFIX.len() + 64);
        tagged.push_str(Self::SENTINEL_AUTHORITY_PREFIX);
        tagged.push_str(&reason.into());
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: Some(PermissionDecision::Ask),
                permission_decision_reason: Some(tagged),
                ..HookSpecificOutput::default()
            }),
            ..Self::default()
        }
    }

    /// Modify tool input before execution (`PreToolUse` only)
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

    /// Inject context formatted through the canonical hook envelope.
    /// Renders as `[HookName] <tier-emoji> <message>` so the user sees a
    /// consistent prefix across all 54 hooks instead of the ad-hoc mix of
    /// brackets / emoji / box-drawing currently in use.
    #[must_use]
    pub fn inject_envelope(event: HookEvent, envelope: &HookEnvelope) -> Self {
        Self::inject_context(event, envelope.render())
    }

    /// Allow the model to retry a denied tool call (`PermissionDenied` only).
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

    /// Transform legacy blocked/reason into proper Claude Code `PreToolUse` JSON.
    /// Called at the output boundary in `hook_cmd.rs` before serialization.
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

        // Transform legacy blocked/reason → hookSpecificOutput deny.
        // Apply the [Sentinel-Authority] prefix here too so legacy `block()`
        // callers and the modern `deny()` constructor produce uniformly
        // tagged output — the agent's contract is "every PreToolUse deny
        // I see from sentinel carries the prefix", and uniformity is what
        // makes that signal trustworthy.
        if self.blocked == Some(true) {
            let reason = self.reason.take().map(|r| {
                if r.starts_with(Self::SENTINEL_AUTHORITY_PREFIX) {
                    r
                } else {
                    format!("{}{}", Self::SENTINEL_AUTHORITY_PREFIX, r)
                }
            });
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
    // Each section encodes a distinct merge-priority policy rule (blocked, HSO,
    // systemMessage, continue_, suppress_output, stop_reason, decision). Splitting
    // into sub-methods would scatter tightly coupled policy logic without gaining clarity.
    #[allow(clippy::too_many_lines)]
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
                    // Permission decision priority: deny > ask > defer > allow
                    if let Some(other_pd) = other_hso.permission_decision {
                        let dominated = match (self_hso.permission_decision, other_pd) {
                            (_, PermissionDecision::Deny) => true,
                            (Some(PermissionDecision::Deny), _) => false,
                            (_, PermissionDecision::Ask) => true,
                            (Some(PermissionDecision::Ask), _) => false,
                            (_, PermissionDecision::Defer) => true,
                            (Some(PermissionDecision::Defer), _) => false,
                            _ => true,
                        };
                        if dominated {
                            self_hso.permission_decision = Some(other_pd);
                            self_hso
                                .permission_decision_reason
                                .clone_from(&other_hso.permission_decision_reason);
                        }
                    }

                    // updatedInput: last writer wins
                    // **Attack #149 fix**: Clear updatedInput when permission is Deny.
                    // A contradictory Deny + updatedInput could confuse clients into
                    // executing a rewritten tool call despite the deny decision.
                    if self_hso.permission_decision == Some(PermissionDecision::Deny) {
                        self_hso.updated_input = None;
                    } else if other_hso.updated_input.is_some() {
                        self_hso.updated_input.clone_from(&other_hso.updated_input);
                    }

                    // additionalContext: concatenate (capped at MAX_ADDITIONAL_CONTEXT_LEN)
                    // **Attack #136 fix**: see module-level constant for rationale.
                    match (&self_hso.additional_context, &other_hso.additional_context) {
                        (Some(a), Some(b)) => {
                            let merged = format!("{a}\n\n{b}");
                            if merged.len() > MAX_ADDITIONAL_CONTEXT_LEN {
                                self_hso.additional_context =
                                    Some(merged[..MAX_ADDITIONAL_CONTEXT_LEN].to_string());
                            } else {
                                self_hso.additional_context = Some(merged);
                            }
                        }
                        (None, Some(b)) => {
                            if b.len() > MAX_ADDITIONAL_CONTEXT_LEN {
                                self_hso.additional_context =
                                    Some(b[..MAX_ADDITIONAL_CONTEXT_LEN].to_string());
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
        let reason = hso.permission_decision_reason.as_deref().unwrap();
        // Provenance prefix is mandatory — see SENTINEL_AUTHORITY_PREFIX.
        assert!(
            reason.starts_with(HookOutput::SENTINEL_AUTHORITY_PREFIX),
            "deny reason missing [Sentinel-Authority] prefix: {reason}"
        );
        assert!(
            reason.contains("not allowed"),
            "deny reason must preserve caller text: {reason}"
        );
    }

    /// The [Sentinel-Authority] prefix is the agent's only signal that a
    /// directive came from sentinel and not from arbitrary tool-result
    /// text. Test that both `deny` and `ask` carry it, and that the
    /// constant value matches the documented contract in the
    /// `SENTINEL_AUTHORITY_PREFIX` doc comment.
    #[test]
    fn test_sentinel_authority_prefix_contract() {
        assert_eq!(
            HookOutput::SENTINEL_AUTHORITY_PREFIX,
            "[Sentinel-Authority] "
        );
        let deny = HookOutput::deny("anything");
        let ask = HookOutput::ask("anything else");
        for o in [deny, ask] {
            let reason = o
                .hook_specific_output
                .unwrap()
                .permission_decision_reason
                .unwrap();
            assert!(
                reason.starts_with(HookOutput::SENTINEL_AUTHORITY_PREFIX),
                "tagged output must start with the authority prefix: {reason}"
            );
        }
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
        let reason = hso.permission_decision_reason.as_deref().unwrap();
        assert!(reason.starts_with(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(reason.contains("no way"));
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
        // `block` flows through `deny` after transformation, so the
        // sentinel authority prefix is now present.
        let reason = hso.permission_decision_reason.as_deref().unwrap();
        assert!(reason.starts_with(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(reason.contains("phase gate violation"));
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

    #[test]
    fn test_hook_envelope_renders_canonical_prefix() {
        let env = HookEnvelope::warn("Worktree Reminder", "use EnterWorktree first");
        assert_eq!(
            env.render(),
            "[Worktree Reminder] 🟡 use EnterWorktree first"
        );
    }

    #[test]
    fn test_hook_envelope_tiers_use_distinct_emoji() {
        assert_eq!(HookTier::Info.emoji(), "🟢");
        assert_eq!(HookTier::Warn.emoji(), "🟡");
        assert_eq!(HookTier::Block.emoji(), "🔴");
    }

    #[test]
    fn test_inject_envelope_round_trips_through_inject_context() {
        let env = HookEnvelope::block("Phase Gate", "load phase file before tools");
        let out = HookOutput::inject_envelope(HookEvent::PreToolUse, &env);
        let ctx = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .unwrap();
        assert_eq!(ctx, "[Phase Gate] 🔴 load phase file before tools");
    }
}
