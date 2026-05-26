//! Activity Tracker — Two-phase hook
//!
//! **`PostToolUse` phase:** Logs every tool call to
//! `~/.claude/metrics/activity-log.jsonl` with structured metadata.
//!
//! **`UserPromptSubmit` phase:** When context is elevated (Yellow+ zone),
//! injects a compact session activity summary to help Claude stay oriented.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{EnvPort, FileSystemPort, HookContext};

/// Cooldown between activity summaries.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_LONG_MS;

/// Minimum tool calls before injecting a summary.
const MIN_CALLS_FOR_SUMMARY: usize = constants::ACTIVITY_TRACKER_MIN_CALLS;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ActivityEntry {
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_action: Option<String>,
    /// **Telemetry gap fix (2026-05-06)**: Skill name when `tool == "Skill"`.
    /// Previously activity-log.jsonl recorded `{"tool":"Skill"}` with no
    /// indication of *which* skill was invoked, making "which skills get
    /// used most" impossible to answer without inferring from MCP traffic
    /// (linear -> linear skill, doppler -> doppler skill, etc — only
    /// works when a skill maps cleanly to one MCP server, which most
    /// don't). Now captured directly from `tool_input.skill`.
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<String>,
    session_id: String,
    ts: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ActivitySummary {
    session_id: String,
    total_calls: usize,
    tool_counts: Vec<(String, usize)>,
    files_touched: Vec<String>,
    mcp_actions: Vec<String>,
    git_commands: Vec<String>,
    ts: String,
}

fn now_ms() -> u64 {
    #[allow(clippy::cast_possible_truncation)] // millis since epoch fit in u64 for centuries
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    ms
}

fn metrics_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir)
}

fn log_file(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    Some(metrics_dir(fs)?.join("activity-log.jsonl"))
}

fn summary_file(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    Some(metrics_dir(fs)?.join(format!("activity-summary-{session_id}.json")))
}

fn cooldown_file(env: &dyn EnvPort) -> PathBuf {
    let session_id = env
        .var("CLAUDE_SESSION_ID")
        .or_else(|| env.var("SESSION_ID"))
        .unwrap_or_else(|| "default".to_string());
    std::env::temp_dir().join(format!("claude-activity-tracker-{session_id}-last"))
}

fn cooldown_expired(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> bool {
    let Ok(content) = fs.read_to_string(&cooldown_file(env)) else {
        return true;
    };
    let Ok(last) = content.trim().parse::<u64>() else {
        return true;
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort, env: &dyn EnvPort) {
    let _ = fs.write(&cooldown_file(env), now_ms().to_string().as_bytes());
}

/// Extract file path from tool input (Edit, Write, Read, Glob, Grep).
fn extract_file_path(tool: &str, input: &serde_json::Value) -> Option<String> {
    match tool {
        "Edit" | "Write" | "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string),
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string),
        "Grep" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string),
        _ => None,
    }
}

/// Truncate `s` to at most `max_bytes`, backing off to the nearest valid
/// UTF-8 char boundary so we never split a multi-byte character.
///
/// **Why:** plain byte-slicing (`&s[..n]`) panics when `n` falls inside a
/// multi-byte char (e.g., em-dash `—` is 3 bytes). Commit messages, task
/// descriptions, and skill names routinely contain such characters.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Extract command from Bash tool input (truncated).
fn extract_command(input: &serde_json::Value) -> Option<String> {
    input.get("command").and_then(|v| v.as_str()).map(|s| {
        if s.len() > 120 {
            format!("{}...", truncate_to_char_boundary(s, 120))
        } else {
            s.to_string()
        }
    })
}

/// Extract the skill name from a `Skill` tool call's `tool_input`.
///
/// Claude Code sends `{"skill": "<name>", "args": "..."}` as the `tool_input`
/// for the Skill tool. Returns None for non-Skill tools or when `skill`
/// is missing/non-string (defensive — malformed input shouldn't crash
/// the activity tracker).
fn extract_skill_name(tool: &str, input: &serde_json::Value) -> Option<String> {
    if tool != "Skill" {
        return None;
    }
    input
        .get("skill")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
}

/// Extract MCP server and action from tool name like `mcp__linear__create_issue`.
fn extract_mcp_info(tool: &str) -> Option<(String, String)> {
    if !tool.starts_with("mcp__") {
        return None;
    }
    let parts: Vec<&str> = tool.splitn(3, "__").collect();
    if parts.len() >= 3 {
        Some((parts[1].to_string(), parts[2].to_string()))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// PostToolUse phase: log every tool call
// ---------------------------------------------------------------------------

pub fn process_post_tool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.clone(),
        None => return HookOutput::allow(),
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown").to_string();
    let tool_input = input.tool_input.clone().unwrap_or_default();

    let file_path = extract_file_path(&tool, &tool_input);
    let command = if tool == "Bash" {
        extract_command(&tool_input)
    } else {
        None
    };
    let (mcp_server, mcp_action) = extract_mcp_info(&tool)
        .map_or((None, None), |(s, a)| (Some(s), Some(a)));
    let skill = extract_skill_name(&tool, &tool_input);

    let entry = ActivityEntry {
        tool,
        file_path,
        command,
        mcp_server,
        mcp_action,
        skill,
        session_id,
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = log_file(ctx.fs) {
        let line = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());
        let _ = ctx.fs.append(&path, line.as_bytes());
    }

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// Stop phase: build session summary for UserPromptSubmit to read
// ---------------------------------------------------------------------------

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    let Some(path) = log_file(ctx.fs) else {
        return HookOutput::allow();
    };

    let Ok(content) = ctx.fs.read_to_string(&path) else {
        return HookOutput::allow();
    };

    // Filter entries for this session
    let entries: Vec<ActivityEntry> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|e: &ActivityEntry| e.session_id == session_id)
        .collect();

    if entries.is_empty() {
        return HookOutput::allow();
    }

    // Aggregate
    let mut tool_counts: HashMap<String, usize> = HashMap::new();
    let mut files: Vec<String> = Vec::new();
    let mut mcp_actions: Vec<String> = Vec::new();
    let mut git_commands: Vec<String> = Vec::new();

    for entry in &entries {
        // Normalize MCP tools to server level for counting
        let count_key = entry
            .mcp_server
            .as_deref()
            .map_or_else(|| entry.tool.clone(), |server| format!("mcp__{server}__*"));
        *tool_counts.entry(count_key).or_insert(0) += 1;

        if let Some(ref fp) = entry.file_path {
            if !files.contains(fp) {
                files.push(fp.clone());
            }
        }

        if let Some(ref action) = entry.mcp_action {
            let desc = format!("{}:{}", entry.mcp_server.as_deref().unwrap_or("?"), action);
            if !mcp_actions.contains(&desc) {
                mcp_actions.push(desc);
            }
        }

        if let Some(ref cmd) = entry.command {
            if cmd.contains("git ") {
                let short = if cmd.len() > 80 {
                    format!("{}...", truncate_to_char_boundary(cmd, 80))
                } else {
                    cmd.clone()
                };
                if !git_commands.contains(&short) {
                    git_commands.push(short);
                }
            }
        }
    }

    let mut sorted_counts: Vec<(String, usize)> = tool_counts.into_iter().collect();
    sorted_counts.sort_by_key(|&(_, count)| Reverse(count));

    let summary = ActivitySummary {
        session_id: session_id.to_string(),
        total_calls: entries.len(),
        tool_counts: sorted_counts,
        files_touched: files.into_iter().take(30).collect(),
        mcp_actions: mcp_actions.into_iter().take(20).collect(),
        git_commands: git_commands.into_iter().take(10).collect(),
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = summary_file(ctx.fs, session_id) {
        let _ = ctx.fs.write(
            &path,
            serde_json::to_string(&summary)
                .unwrap_or_default()
                .as_bytes(),
        );
    }

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject activity summary when context is elevated
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    let Some(path) = summary_file(ctx.fs, session_id) else {
        return HookOutput::allow();
    };

    let Ok(content) = ctx.fs.read_to_string(&path) else {
        return HookOutput::allow();
    };

    let summary: ActivitySummary = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Only inject for the current session
    if summary.session_id != session_id {
        return HookOutput::allow();
    }

    // Don't inject for short sessions
    if summary.total_calls < MIN_CALLS_FOR_SUMMARY {
        return HookOutput::allow();
    }

    // Check if context is elevated (read from context-zone.json)
    let in_elevated_zone = check_elevated_context(ctx.fs, session_id);

    // Only inject when context is elevated OR there are a LOT of tool calls
    if !in_elevated_zone && summary.total_calls < 50 {
        return HookOutput::allow();
    }

    if !cooldown_expired(ctx.fs, ctx.env) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs, ctx.env);

    let ctx_text = build_summary_context(&summary);
    HookOutput::inject_context(HookEvent::UserPromptSubmit, ctx_text)
}

/// Check if context monitor reported Yellow+ zone for this session.
fn check_elevated_context(fs: &dyn FileSystemPort, session_id: &str) -> bool {
    let Some(path) = metrics_dir(fs).map(|d| d.join(format!("context-zone-{session_id}.json")))
    else {
        return false;
    };
    let Ok(content) = fs.read_to_string(&path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };

    // Must be same session
    if val.get("session_id").and_then(|v| v.as_str()) != Some(session_id) {
        return false;
    }

    let pct = val
        .get("percent_used")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    pct >= 50.0
}

fn build_summary_context(summary: &ActivitySummary) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "[Activity Tracker] Session activity: {} tool calls.",
        summary.total_calls
    ));

    // Tool breakdown (top 6)
    let tool_breakdown: String = summary
        .tool_counts
        .iter()
        .take(6)
        .map(|(tool, count)| format!("{tool}: {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("Tools: {tool_breakdown}"));

    // Files touched (top 10)
    if !summary.files_touched.is_empty() {
        let file_names: Vec<String> = summary
            .files_touched
            .iter()
            .take(10)
            .map(|f| {
                // Show just filename, not full path
                f.rsplit(['/', '\\']).next().unwrap_or(f).to_string()
            })
            .collect();
        let extra = if summary.files_touched.len() > 10 {
            format!(" (+{} more)", summary.files_touched.len() - 10)
        } else {
            String::new()
        };
        lines.push(format!("Files: {}{extra}", file_names.join(", ")));
    }

    // Git activity
    if !summary.git_commands.is_empty() {
        let git_summary: String = summary
            .git_commands
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(format!("Git: {git_summary}"));
    }

    // MCP actions (top 5)
    if !summary.mcp_actions.is_empty() {
        let mcp_summary: String = summary
            .mcp_actions
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("MCP: {mcp_summary}"));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_file_path_edit() {
        let input = json!({"file_path": "/src/main.rs"});
        assert_eq!(
            extract_file_path("Edit", &input),
            Some("/src/main.rs".into())
        );
    }

    #[test]
    fn test_extract_file_path_glob() {
        let input = json!({"pattern": "**/*.rs"});
        assert_eq!(extract_file_path("Glob", &input), Some("**/*.rs".into()));
    }

    #[test]
    fn test_extract_file_path_none() {
        let input = json!({"command": "ls"});
        assert_eq!(extract_file_path("Bash", &input), None);
    }

    #[test]
    fn test_extract_command() {
        let input = json!({"command": "cargo test"});
        assert_eq!(extract_command(&input), Some("cargo test".into()));
    }

    #[test]
    fn test_extract_command_truncated() {
        let long_cmd = "a".repeat(200);
        let input = json!({"command": long_cmd});
        let result = extract_command(&input).unwrap();
        assert!(result.len() < 130);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_to_char_boundary_handles_multibyte() {
        // Em-dash is 3 bytes in UTF-8: E2 80 94. String layout:
        //   'a' 'b' 'c'   —(3 bytes)   'd' 'e' 'f'
        //    0   1   2    3  4  5      6   7   8
        let s = "abc—def";
        assert_eq!(s.len(), 9);

        // Cutting at byte 4 would split the em-dash → back off to byte 3
        assert_eq!(truncate_to_char_boundary(s, 4), "abc");
        // Cutting at byte 5 also splits → back off to byte 3
        assert_eq!(truncate_to_char_boundary(s, 5), "abc");
        // Byte 6 is the boundary just after the em-dash
        assert_eq!(truncate_to_char_boundary(s, 6), "abc—");
        // Beyond length → return whole string
        assert_eq!(truncate_to_char_boundary(s, 100), s);
        // Edge: zero
        assert_eq!(truncate_to_char_boundary(s, 0), "");
    }

    #[test]
    fn test_extract_command_truncated_with_multibyte_char() {
        // Regression: a commit message containing an em-dash at byte
        // position 79-81 used to panic on `&s[..80]` because byte 80
        // is mid em-dash. Reported at activity_tracker.rs:239 (now :266).
        let mut cmd =
            "git add -A && git commit -m \"docs(audit pass 3): CHANGELOG entry ".to_string();
        // Pad to put an em-dash spanning the 120-byte boundary
        while cmd.len() < 119 {
            cmd.push('a');
        }
        cmd.push('—'); // bytes 119..122
        cmd.push_str(" full Pass-3 batch\"");
        assert!(
            cmd.len() > 120,
            "test setup: cmd must exceed truncation threshold"
        );
        let input = json!({"command": cmd});
        // Must not panic.
        let result = extract_command(&input).unwrap();
        // Output is valid UTF-8 (Rust strings always are; this asserts
        // the format!() didn't panic mid-construction).
        assert!(result.ends_with("..."));
        // Truncation backed off to before the em-dash (byte 119), so the
        // body before "..." is 119 bytes of ASCII; total = 119 + 3.
        assert_eq!(result.len(), 119 + 3);
        // The em-dash itself must NOT appear in the truncated result.
        assert!(!result.contains('—'));
    }

    #[test]
    fn test_extract_mcp_info() {
        let (server, action) = extract_mcp_info("mcp__linear__create_issue").unwrap();
        assert_eq!(server, "linear");
        assert_eq!(action, "create_issue");
    }

    #[test]
    fn test_extract_mcp_info_none() {
        assert!(extract_mcp_info("Edit").is_none());
        assert!(extract_mcp_info("Bash").is_none());
    }

    // ── Telemetry gap fix (2026-05-06): skill-name capture ─────────────

    #[test]
    fn test_extract_skill_name_captures_skill_arg() {
        // The whole point of this fix: when tool=="Skill", we must
        // record which skill was invoked instead of dropping the info.
        let input = json!({"skill": "linear", "args": "ship FPCRM-429"});
        assert_eq!(extract_skill_name("Skill", &input), Some("linear".into()));
    }

    #[test]
    fn test_extract_skill_name_returns_none_for_non_skill_tools() {
        // Non-Skill tools must never have a skill field populated, even
        // if their tool_input happens to contain a `skill` key (defensive
        // — wouldn't want a synthetic skill arg on a Bash call to muddy
        // the activity log).
        let input = json!({"skill": "linear"});
        assert!(extract_skill_name("Bash", &input).is_none());
        assert!(extract_skill_name("Edit", &input).is_none());
        assert!(extract_skill_name("mcp__linear__create_issue", &input).is_none());
    }

    #[test]
    fn test_extract_skill_name_returns_none_when_skill_missing() {
        // Malformed Skill tool_input (no `skill` key, or non-string)
        // shouldn't crash the activity tracker — return None gracefully.
        assert!(extract_skill_name("Skill", &json!({})).is_none());
        assert!(extract_skill_name("Skill", &json!({"args": "foo"})).is_none());
        // Non-string skill value: still None.
        assert!(extract_skill_name("Skill", &json!({"skill": 123})).is_none());
        assert!(extract_skill_name("Skill", &json!({"skill": null})).is_none());
    }

    #[test]
    fn test_activity_entry_serializes_skill_when_present() {
        // The serde representation must include `"skill":"<name>"` so
        // downstream tools (telemetry dashboards, M7 router corpus
        // queries) can group by skill without re-deriving from MCP
        // traffic patterns.
        let entry = ActivityEntry {
            tool: "Skill".into(),
            file_path: None,
            command: None,
            mcp_server: None,
            mcp_action: None,
            skill: Some("linear".into()),
            session_id: "sess-1".into(),
            ts: "2026-05-06T17:00:00Z".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""skill":"linear""#), "got: {json}");
        assert!(json.contains(r#""tool":"Skill""#), "got: {json}");
    }

    #[test]
    fn test_activity_entry_omits_skill_field_when_none() {
        // For non-Skill tool calls, `skill` must NOT appear in the JSON
        // (skip_serializing_if). Old log entries that don't have a skill
        // field stay parseable, and new entries don't bloat the log
        // file with empty skill: null on every line.
        let entry = ActivityEntry {
            tool: "Edit".into(),
            file_path: Some("/tmp/foo.rs".into()),
            command: None,
            mcp_server: None,
            mcp_action: None,
            skill: None,
            session_id: "sess-1".into(),
            ts: "2026-05-06T17:00:00Z".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("skill"),
            "skill field must be omitted when None, got: {json}"
        );
    }

    #[test]
    fn test_post_tool_no_tool_name() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_post_tool_logs_entry() {
        let input = HookInput {
            tool_name: Some("Edit".into()),
            tool_input: Some(json!({"file_path": "/test/file.rs"})),
            session_id: Some("test-activity".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_build_summary_context() {
        let summary = ActivitySummary {
            session_id: "test".into(),
            total_calls: 42,
            tool_counts: vec![
                ("Edit".into(), 15),
                ("Read".into(), 10),
                ("Bash".into(), 8),
                ("Grep".into(), 5),
                ("mcp__linear__*".into(), 4),
            ],
            files_touched: vec!["src/main.rs".into(), "README.md".into()],
            mcp_actions: vec!["linear:create_issue".into()],
            git_commands: vec!["git commit -m \"feat: stuff\"".into()],
            ts: "2026-03-05".into(),
        };

        let ctx = build_summary_context(&summary);
        assert!(ctx.contains("42 tool calls"));
        assert!(ctx.contains("Edit: 15"));
        assert!(ctx.contains("main.rs"));
        assert!(ctx.contains("linear:create_issue"));
        assert!(ctx.contains("git commit"));
    }

    #[test]
    fn test_prompt_no_summary_returns_allow() {
        let input = HookInput {
            session_id: Some("nonexistent-session-xyz".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_cooldown_logic() {
        let ctx = crate::hooks::test_support::stub_ctx();
        // StubFs returns error on read → expired
        assert!(cooldown_expired(ctx.fs, ctx.env));
    }

    #[test]
    fn test_stop_empty_session_returns_allow() {
        let input = HookInput {
            session_id: Some("empty-session-no-activity".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
