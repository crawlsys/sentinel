//! Skill Invocation Gate — enforces that detected skills are actually invoked.
//!
//! When `skill_router` detects a skill, it injects a `MANDATORY: ... Read(...)`
//! message asking Claude to load the skill before continuing. That instruction
//! is advisory — Claude can ignore it and call other tools without ever
//! invoking the skill. This hook makes the rule binding:
//!
//! 1. **PreToolUse** — if a pending-skill state file exists for the session,
//!    block any tool call outside the read-only allowlist with a clear "load
//!    the skill first" message. The allowlist includes Read/Glob/Grep so
//!    Claude can navigate to SKILL.md, plus `Skill` itself so the invocation
//!    that satisfies the gate isn't blocked by it.
//!
//! 2. **PostToolUse** — when the `Skill` tool fires with a name matching the
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

fn load_pending_state(
    fs: &dyn FileSystemPort,
    session_id: &str,
) -> Option<PendingSkillState> {
    let path = super::skill_router::pending_skill_state_path(fs, session_id)?;
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn clear_pending_state(fs: &dyn FileSystemPort, session_id: &str) {
    if let Some(path) = super::skill_router::pending_skill_state_path(fs, session_id) {
        // Best-effort delete via overwrite-and-ignore; FileSystemPort doesn't
        // expose remove(). Writing empty content makes load_pending_state
        // return None on next read since serde_json::from_str("") fails.
        let _ = fs.write(&path, b"");
    }
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

/// PreToolUse handler — block when there's a pending skill and the tool
/// isn't on the allowlist.
pub fn process_pretool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
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
            state.skill,
            state.skill_path,
            tool_name,
        ),
    );
    HookOutput::block(envelope.render())
}

/// PostToolUse handler — clear the pending state once the `Skill` tool has
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
        .map(|s| s == expected_skill)
        .unwrap_or(false)
}

/// True when the Read tool's `file_path` looks like the SKILL.md for the
/// pending skill. Compares both the skill_path string (with tilde expanded
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
}
