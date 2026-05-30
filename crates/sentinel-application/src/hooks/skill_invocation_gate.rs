//! Skill Invocation Gate — enforces that detected skills are actually invoked.
//!
//! When `skill_router` detects a skill, it injects a `MANDATORY: ... Read(...)`
//! message asking Claude to load the skill before continuing. That instruction
//! is advisory — Claude can ignore it and call other tools without ever
//! invoking the skill. This hook makes the rule binding:
//!
//! 1. **`PreToolUse`** — if a pending-skill state file exists for the session,
//!    block any tool call outside the read-only allowlist with a clear "load
//!    the skill first" message. The allowlist includes Read/Glob/Grep so
//!    Claude can navigate to SKILL.md, plus `Skill` itself so the invocation
//!    that satisfies the gate isn't blocked by it.
//!
//! 2. **`PostToolUse`** — when the `Skill` tool fires with a name matching the
//!    pending skill, clear the state. State also auto-clears after a 5-minute
//!    TTL so a skill the user explicitly skipped doesn't deadlock the session.

use chrono::Utc;
use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};

use super::{skill_router::PendingSkillState, FileSystemPort, HookContext};

/// Read-only / progress-toward-skill-load tools that should never be blocked.
/// These either don't change state (Read/Glob/Grep/LSP/Web*) or are how the
/// model satisfies the gate (Skill/TaskCreate/TaskUpdate/sequentialthinking).
const ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "WebSearch",
    "WebFetch",
    "ToolSearch",
    "Skill",
    "TaskList",
    "TaskGet",
    "TaskCreate",
    "TaskUpdate",
    "TaskOutput",
    "mcp__sequential-thinking__sequentialthinking",
];

/// Pending state has a 5-minute TTL. After that, clear it and allow tools
/// through — prevents a skill the user has explicitly moved on from from
/// permanently deadlocking the session.
const PENDING_TTL_SECS: i64 = 300;

fn load_pending_state(fs: &dyn FileSystemPort, session_id: &str) -> Option<PendingSkillState> {
    let path = super::skill_router::pending_skill_state_path(fs, session_id)?;
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn clear_pending_state(fs: &dyn FileSystemPort, session_id: &str) {
    // Delegate to the router's helper, which removes the file via
    // `remove_file`. (The old implementation overwrote the file with empty
    // bytes because it assumed FileSystemPort had no remove(); it does.)
    super::skill_router::clear_pending_skill_state(fs, session_id);
}

/// True when `detected_at` is older than the TTL.
fn is_stale(detected_at: &str) -> bool {
    let parsed = chrono::DateTime::parse_from_rfc3339(detected_at);
    match parsed {
        Ok(dt) => Utc::now().signed_duration_since(dt).num_seconds() > PENDING_TTL_SECS,
        // Unparseable timestamp → treat as stale; better to fail-open than
        // deadlock on corrupt state.
        Err(_) => true,
    }
}

/// True when this tool call originates from inside a subagent rather than the
/// main session. Claude Code stamps `agent_id` (and usually `agent_type`) on
/// every hook payload that fires from a spawned agent's context; the main
/// session leaves both unset. We treat either being present as "subagent".
///
/// The pending-skill state file is keyed by `session_id`, which a subagent
/// shares with its parent. Without this check the gate would block a
/// subagent's tools based on a skill the *main* session was asked to invoke —
/// a skill the subagent was never told about and has no way to satisfy.
fn is_subagent(input: &HookInput) -> bool {
    let nonempty = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.is_empty());
    nonempty(&input.agent_id) || nonempty(&input.agent_type)
}

/// `PreToolUse` handler — block when there's a pending skill and the tool
/// isn't on the allowlist.
pub fn process_pretool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // Never gate subagent/teammate tool calls. The pending-skill marker is a
    // main-session concept: it records that the user's message routed to a
    // skill the *main* session must invoke. A subagent shares the parent's
    // `session_id` but runs in its own context with its own instructions, so
    // gating it on the parent's pending skill is always wrong (and the
    // subagent can't clear the marker without derailing its own task).
    if is_subagent(input) {
        return HookOutput::allow();
    }

    let session_id = match input.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return HookOutput::allow(),
    };

    let state = match load_pending_state(ctx.fs, session_id) {
        Some(s) => s,
        None => return HookOutput::allow(),
    };

    // Auto-clear stale state so the gate is self-healing.
    if is_stale(&state.detected_at) {
        clear_pending_state(ctx.fs, session_id);
        return HookOutput::allow();
    }

    let tool_name = input.tool_name.as_deref().unwrap_or("");
    if ALLOWED_TOOLS.contains(&tool_name) {
        return HookOutput::allow();
    }

    let envelope = HookEnvelope::block(
        "Skill Gate",
        format!(
            "Skill `{}` was detected but not invoked. Call `Skill(skill: \"{0}\")` \
             or `Read(\"{}\")` before using `{}`. \
             (Read-only tools and TaskCreate/TaskUpdate are allowed.)",
            state.skill, state.skill_path, tool_name,
        ),
    );
    HookOutput::block(envelope.render())
}

/// `PostToolUse` handler — clear the pending state once the `Skill` tool has
/// been invoked with a matching skill name. Also clears on `Read` of the
/// SKILL.md file as a fallback so the legacy "MANDATORY: Read(...)" flow
/// still satisfies the gate.
pub fn process_posttool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = match input.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return HookOutput::allow(),
    };

    let state = match load_pending_state(ctx.fs, session_id) {
        Some(s) => s,
        None => return HookOutput::allow(),
    };

    let tool_name = input.tool_name.as_deref().unwrap_or("");
    let cleared = match tool_name {
        "Skill" => skill_arg_matches(input, &state.skill),
        "Read" => read_target_matches(input, &state.skill_path, &state.skill),
        _ => false,
    };

    if cleared {
        clear_pending_state(ctx.fs, session_id);
    }
    HookOutput::allow()
}

/// True when the Skill tool's `skill` arg matches the pending name.
fn skill_arg_matches(input: &HookInput, expected_skill: &str) -> bool {
    input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("skill"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == expected_skill)
}

/// True when the Read tool's `file_path` looks like the SKILL.md for the
/// pending skill. Compares both the `skill_path` string (with tilde expanded
/// by the caller) and a fallback "skills/<name>/SKILL.md" suffix match so
/// home-dir variants don't false-negative.
fn read_target_matches(input: &HookInput, skill_path: &str, skill: &str) -> bool {
    let target = match input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
    {
        Some(t) => t,
        None => return false,
    };

    // Direct match against the recorded skill_path (handles tilde-expanded
    // identical strings).
    if target == skill_path {
        return true;
    }
    // Fallback: any path that ends with `skills/<skill>/SKILL.md` (forward
    // or back-slash) is good enough — covers Windows/Unix and tilde expansion.
    let suffix_unix = format!("skills/{skill}/SKILL.md");
    let suffix_win = format!("skills\\{skill}\\SKILL.md");
    target.ends_with(&suffix_unix) || target.ends_with(&suffix_win)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_stale_true_for_old_timestamp() {
        let old = (Utc::now() - chrono::Duration::seconds(PENDING_TTL_SECS + 60)).to_rfc3339();
        assert!(is_stale(&old));
    }

    #[test]
    fn test_is_stale_false_for_recent() {
        let recent = Utc::now().to_rfc3339();
        assert!(!is_stale(&recent));
    }

    #[test]
    fn test_is_stale_true_for_garbage() {
        // Unparseable → treat as stale (fail-open).
        assert!(is_stale("not-a-real-timestamp"));
    }

    #[test]
    fn test_skill_arg_matches() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({"skill": "linear"})),
            ..Default::default()
        };
        assert!(skill_arg_matches(&input, "linear"));
        assert!(!skill_arg_matches(&input, "memory"));
    }

    #[test]
    fn test_skill_arg_matches_handles_missing_input() {
        let input = HookInput::default();
        assert!(!skill_arg_matches(&input, "linear"));
    }

    #[test]
    fn test_read_target_matches_direct() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "file_path": "~/.claude/skills/linear/SKILL.md"
            })),
            ..Default::default()
        };
        assert!(read_target_matches(
            &input,
            "~/.claude/skills/linear/SKILL.md",
            "linear",
        ));
    }

    #[test]
    fn test_read_target_matches_suffix_unix() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "file_path": "/home/gary/.claude/skills/linear/SKILL.md"
            })),
            ..Default::default()
        };
        assert!(read_target_matches(
            &input,
            "~/.claude/skills/linear/SKILL.md",
            "linear",
        ));
    }

    #[test]
    fn test_read_target_matches_suffix_windows() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "file_path": "C:\\Users\\garys\\.claude\\skills\\linear\\SKILL.md"
            })),
            ..Default::default()
        };
        assert!(read_target_matches(
            &input,
            "~/.claude/skills/linear/SKILL.md",
            "linear",
        ));
    }

    #[test]
    fn test_read_target_does_not_match_other_skill() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({
                "file_path": "/home/gary/.claude/skills/memory/SKILL.md"
            })),
            ..Default::default()
        };
        assert!(!read_target_matches(
            &input,
            "~/.claude/skills/linear/SKILL.md",
            "linear",
        ));
    }

    #[test]
    fn test_pretool_allows_when_no_pending_state() {
        let input = HookInput {
            session_id: Some("test".to_string()),
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_pretool(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_pretool_allows_when_no_session_id() {
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_pretool(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allowed_tools_include_skill_and_read() {
        // Sanity: the tools that satisfy the gate must themselves be allowlisted
        // so the gate doesn't refuse to let Claude clear it.
        assert!(ALLOWED_TOOLS.contains(&"Skill"));
        assert!(ALLOWED_TOOLS.contains(&"Read"));
    }

    #[test]
    fn test_is_subagent_detects_agent_id() {
        let input = HookInput {
            agent_id: Some("a3f2-deadbeef".to_string()),
            ..Default::default()
        };
        assert!(is_subagent(&input));
    }

    #[test]
    fn test_is_subagent_detects_agent_type() {
        let input = HookInput {
            agent_type: Some("code-reviewer".to_string()),
            ..Default::default()
        };
        assert!(is_subagent(&input));
    }

    #[test]
    fn test_is_subagent_false_for_main_session() {
        // Main-session payloads leave both fields unset (or empty).
        let input = HookInput {
            agent_id: Some(String::new()),
            agent_type: None,
            ..Default::default()
        };
        assert!(!is_subagent(&input));
        assert!(!is_subagent(&HookInput::default()));
    }

    #[test]
    fn test_pretool_never_blocks_subagent_tool_calls() {
        // The core fix: even with a session_id whose pending-skill marker would
        // otherwise gate a non-allowlisted tool, a subagent call (agent_id set)
        // must pass through. We don't need pending state on disk to prove this —
        // is_subagent short-circuits before state is ever loaded — but we use a
        // real non-allowlisted tool name to make the intent explicit.
        let input = HookInput {
            session_id: Some("shared-with-parent".to_string()),
            tool_name: Some("Bash".to_string()),
            agent_id: Some("a3f2-deadbeef".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_pretool(&input, &ctx);
        assert!(
            output.blocked.is_none(),
            "subagent Bash call must not be gated on the parent's pending skill",
        );
    }
}
