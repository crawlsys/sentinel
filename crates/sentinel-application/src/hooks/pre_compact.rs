//! Pre-Compact Snapshot — PreCompact hook
//!
//! Fires right before Claude Code auto-compacts the context window.
//! Writes a survival snapshot to `~/.claude/metrics/compact-snapshot.json`
//! containing: active skill, phases read, recent files, git branch,
//! tool call count, and activity summary.
//!
//! The `session_resume` skill and `activity_tracker` hook read this
//! snapshot to help Claude recover orientation after compaction.

use sentinel_domain::events::{HookInput, HookOutput};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompactSnapshot {
    session_id: String,
    /// Active skill at time of compaction
    active_skill: Option<String>,
    /// Phase files already read in this session
    phases_read: Vec<String>,
    /// Total tool calls before compaction
    tool_calls: u64,
    /// Current working directory
    cwd: Option<String>,
    /// Git branch (if detectable from cwd)
    git_branch: Option<String>,
    /// Recent files from activity log (top 20)
    recent_files: Vec<String>,
    /// Recent git commands from activity log
    recent_git: Vec<String>,
    /// Context usage % at compaction (from context-zone.json)
    context_percent: Option<f64>,
    /// Compact activity summary (tool counts)
    tool_summary: Vec<(String, usize)>,
    /// Timestamp
    ts: String,
}

fn metrics_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dir = home.join(".claude").join("metrics");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn snapshot_file() -> Option<PathBuf> {
    Some(metrics_dir()?.join("compact-snapshot.json"))
}

/// Read the activity summary written by activity_tracker::process_stop
fn read_activity_summary(session_id: &str) -> (Vec<String>, Vec<String>, Vec<(String, usize)>) {
    let path = match metrics_dir() {
        Some(d) => d.join("activity-summary.json"),
        None => return (Vec::new(), Vec::new(), Vec::new()),
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
    };
    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new(), Vec::new()),
    };

    // Only use if same session
    if val.get("session_id").and_then(|v| v.as_str()) != Some(session_id) {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let files: Vec<String> = val
        .get("files_touched")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .take(20)
                .collect()
        })
        .unwrap_or_default();

    let git: Vec<String> = val
        .get("git_commands")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .take(10)
                .collect()
        })
        .unwrap_or_default();

    let tool_counts: Vec<(String, usize)> = val
        .get("tool_counts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let pair = item.as_array()?;
                    let name = pair.first()?.as_str()?.to_string();
                    let count = pair.get(1)?.as_u64()? as usize;
                    Some((name, count))
                })
                .collect()
        })
        .unwrap_or_default();

    (files, git, tool_counts)
}

/// Read context usage % from context-zone.json
fn read_context_percent(session_id: &str) -> Option<f64> {
    let path = metrics_dir()?.join("context-zone.json");
    let content = fs::read_to_string(&path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;

    if val.get("session_id").and_then(|v| v.as_str()) != Some(session_id) {
        return None;
    }

    val.get("percent_used").and_then(|v| v.as_f64())
}

/// Detect current git branch from cwd
fn detect_git_branch(cwd: &str) -> Option<String> {
    let head_path = std::path::Path::new(cwd).join(".git").join("HEAD");
    let content = fs::read_to_string(head_path).ok()?;
    let trimmed = content.trim();
    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
        Some(branch.to_string())
    } else {
        // Detached HEAD — return short hash
        Some(trimmed.chars().take(8).collect())
    }
}

/// Read session state from the state store
fn read_session_state(session_id: &str) -> (Option<String>, Vec<String>, u64) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return (None, Vec::new(), 0),
    };
    let state_path = home
        .join(".claude")
        .join("sentinel")
        .join("state")
        .join(format!("{session_id}.json"));

    let content = match fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(_) => return (None, Vec::new(), 0),
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return (None, Vec::new(), 0),
    };

    let active_skill = val
        .get("active_skill")
        .and_then(|v| v.as_str())
        .map(String::from);

    let phases_read: Vec<String> = val
        .get("phases_read")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let tool_calls = val
        .get("tool_calls")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    (active_skill, phases_read, tool_calls)
}

// ---------------------------------------------------------------------------
// PreCompact: snapshot session state before context compaction
// ---------------------------------------------------------------------------

pub fn process(input: &HookInput) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref();

    // Read session state
    let (active_skill, phases_read, tool_calls) = read_session_state(session_id);

    // Read activity data
    let (recent_files, recent_git, tool_summary) = read_activity_summary(session_id);

    // Read context %
    let context_percent = read_context_percent(session_id);

    // Detect git branch
    let git_branch = cwd.and_then(detect_git_branch);

    let snapshot = CompactSnapshot {
        session_id: session_id.to_string(),
        active_skill,
        phases_read,
        tool_calls,
        cwd: cwd.map(String::from),
        git_branch,
        recent_files,
        recent_git,
        context_percent,
        tool_summary,
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = snapshot_file() {
        let _ = fs::write(&path, serde_json::to_string_pretty(&snapshot).unwrap_or_default());
    }

    tracing::info!(
        session = session_id,
        skill = ?snapshot.active_skill,
        tool_calls = snapshot.tool_calls,
        "Pre-compact snapshot saved"
    );

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_default_input() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_with_session() {
        let input = HookInput {
            session_id: Some("test-compact-snapshot".into()),
            cwd: Some(".".into()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_snapshot_file_exists() {
        assert!(snapshot_file().is_some());
    }

    #[test]
    fn test_detect_git_branch_no_dir() {
        assert!(detect_git_branch("/nonexistent/path/xyz").is_none());
    }

    #[test]
    fn test_read_session_state_missing() {
        let (skill, phases, calls) = read_session_state("nonexistent-session-xyz");
        assert!(skill.is_none());
        assert!(phases.is_empty());
        assert_eq!(calls, 0);
    }

    #[test]
    fn test_read_activity_summary_missing() {
        let (files, git, counts) = read_activity_summary("nonexistent-session-xyz");
        assert!(files.is_empty());
        assert!(git.is_empty());
        assert!(counts.is_empty());
    }

    #[test]
    fn test_read_context_percent_missing() {
        assert!(read_context_percent("nonexistent-session-xyz").is_none());
    }

    #[test]
    fn test_snapshot_serialization() {
        let snapshot = CompactSnapshot {
            session_id: "test".into(),
            active_skill: Some("linear".into()),
            phases_read: vec!["claim.md".into()],
            tool_calls: 42,
            cwd: Some("/project".into()),
            git_branch: Some("feat/hooks".into()),
            recent_files: vec!["main.rs".into()],
            recent_git: vec!["git commit -m 'test'".into()],
            context_percent: Some(72.5),
            tool_summary: vec![("Edit".into(), 15), ("Read".into(), 10)],
            ts: "2026-03-05".into(),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("linear"));
        assert!(json.contains("claim.md"));
        assert!(json.contains("72.5"));
    }
}
