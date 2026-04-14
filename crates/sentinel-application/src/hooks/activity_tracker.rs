//! Activity Tracker — Two-phase hook
//!
//! **PostToolUse phase:** Logs every tool call to
//! `~/.claude/metrics/activity-log.jsonl` with structured metadata.
//!
//! **UserPromptSubmit phase:** When context is elevated (Yellow+ zone),
//! injects a compact session activity summary to help Claude stay oriented.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{FileSystemPort, HookContext};

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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

fn summary_file(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    Some(metrics_dir(fs)?.join("activity-summary.json"))
}

fn cooldown_file() -> PathBuf {
    std::env::temp_dir().join("claude-activity-tracker-last")
}

fn cooldown_expired(fs: &dyn FileSystemPort) -> bool {
    let content = match fs.read_to_string(&cooldown_file()) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort) {
    let _ = fs.write(&cooldown_file(), now_ms().to_string().as_bytes());
}

/// Extract file path from tool input (Edit, Write, Read, Glob, Grep).
fn extract_file_path(tool: &str, input: &serde_json::Value) -> Option<String> {
    match tool {
        "Edit" | "Write" | "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "Grep" => input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// Extract command from Bash tool input (truncated).
fn extract_command(input: &serde_json::Value) -> Option<String> {
    input.get("command").and_then(|v| v.as_str()).map(|s| {
        if s.len() > 120 {
            format!("{}...", &s[..120])
        } else {
            s.to_string()
        }
    })
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
    let tool_input = input.tool_input.as_ref().cloned().unwrap_or_default();

    let file_path = extract_file_path(&tool, &tool_input);
    let command = if tool == "Bash" {
        extract_command(&tool_input)
    } else {
        None
    };
    let (mcp_server, mcp_action) = extract_mcp_info(&tool)
        .map(|(s, a)| (Some(s), Some(a)))
        .unwrap_or((None, None));

    let entry = ActivityEntry {
        tool,
        file_path,
        command,
        mcp_server,
        mcp_action,
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

    let path = match log_file(ctx.fs) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
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
        let count_key = if let Some(ref server) = entry.mcp_server {
            format!("mcp__{server}__*")
        } else {
            entry.tool.clone()
        };
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
                    format!("{}...", &cmd[..80])
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
    sorted_counts.sort_by(|a, b| b.1.cmp(&a.1));

    let summary = ActivitySummary {
        session_id: session_id.to_string(),
        total_calls: entries.len(),
        tool_counts: sorted_counts,
        files_touched: files.into_iter().take(30).collect(),
        mcp_actions: mcp_actions.into_iter().take(20).collect(),
        git_commands: git_commands.into_iter().take(10).collect(),
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = summary_file(ctx.fs) {
        let _ = ctx.fs.write(
            &path,
            serde_json::to_string(&summary).unwrap_or_default().as_bytes(),
        );
    }

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject activity summary when context is elevated
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    let path = match summary_file(ctx.fs) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
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

    if !cooldown_expired(ctx.fs) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs);

    let context = build_summary_context(&summary);
    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

/// Check if context monitor reported Yellow+ zone for this session.
fn check_elevated_context(fs: &dyn FileSystemPort, session_id: &str) -> bool {
    let path = match metrics_dir(fs) {
        Some(d) => d.join("context-zone.json"),
        None => return false,
    };
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Must be same session
    if val.get("session_id").and_then(|v| v.as_str()) != Some(session_id) {
        return false;
    }

    let pct = val
        .get("percent_used")
        .and_then(|v| v.as_f64())
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
        assert!(cooldown_expired(ctx.fs));
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
