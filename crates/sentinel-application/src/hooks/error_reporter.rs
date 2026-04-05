//! Error Reporter Hook
//!
//! Runs on UserPromptSubmit. Reads `~/.claude/metrics/errors.jsonl` for
//! unresolved infrastructure errors. If any found and cooldown (10 min) has
//! expired, injects context instructing Claude to file Linear issues.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Cooldown between error reports (10 minutes)
const COOLDOWN_MS: u64 = 10 * 60 * 1000;
const MAX_ERRORS_IN_CONTEXT: usize = 3;

/// Linear workspace config for auto-filing — loaded from config file at runtime
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct LinearConfig {
    account: String,
    team_id: String,
    state_id: String,
    assignee_id: String,
    label_bug: String,
    label_infrastructure: String,
    label_auto_filed: String,
}

impl Default for LinearConfig {
    fn default() -> Self {
        Self {
            account: "personal".to_string(),
            team_id: String::new(),
            state_id: String::new(),
            assignee_id: String::new(),
            label_bug: String::new(),
            label_infrastructure: String::new(),
            label_auto_filed: String::new(),
        }
    }
}

/// Load Linear config from ~/.claude/sentinel/config/error-reporter.toml
fn load_linear_config() -> LinearConfig {
    let path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("config")
        .join("error-reporter.toml");

    if let Ok(content) = fs::read_to_string(&path) {
        toml::from_str(&content).unwrap_or_default()
    } else {
        LinearConfig::default()
    }
}

/// A single error entry from errors.jsonl
#[derive(Debug, serde::Deserialize)]
struct ErrorEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    component: String,
    #[serde(default, rename = "type")]
    error_type: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    ts: String,
    /// If present, this error has been resolved
    #[serde(default)]
    resolved: Option<serde_json::Value>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn errors_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("metrics")
        .join("errors.jsonl")
}

fn cooldown_file() -> PathBuf {
    std::env::temp_dir().join("claude-error-reporter-last")
}

/// Read unresolved errors from errors.jsonl
fn read_unresolved_errors(path: &PathBuf) -> Vec<ErrorEntry> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<ErrorEntry>(line).ok())
        .filter(|entry| entry.resolved.is_none())
        .filter(|entry| is_actionable_error(entry))
        .collect()
}

fn is_actionable_error(entry: &ErrorEntry) -> bool {
    if entry.id.trim().is_empty()
        || entry.component.trim().is_empty()
        || entry.severity.trim().is_empty()
        || entry.error.trim().is_empty()
    {
        return false;
    }

    // These are common runtime/session conditions and prompt-size failures, not
    // durable infrastructure defects worth injecting into every prompt.
    !matches!(
        entry.error.as_str(),
        "rate_limit" | "auth_error" | "invalid_request"
    ) && !entry.error.contains("prompt is too long")
}

/// Check if cooldown has expired
fn cooldown_expired(path: &PathBuf) -> bool {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return true, // No cooldown file = expired
    };
    let last_report: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last_report) >= COOLDOWN_MS
}

/// Write cooldown marker
fn write_cooldown(path: &PathBuf) {
    let _ = fs::write(path, now_ms().to_string());
}

/// Process the error-reporter hook event
pub fn process(input: &HookInput) -> HookOutput {
    // Only meaningful on UserPromptSubmit, but we don't gate on that —
    // the engine routes events to the correct hooks.
    let _ = input;

    let errors_path = errors_file();
    let errors = read_unresolved_errors(&errors_path);

    if errors.is_empty() {
        return HookOutput::allow();
    }

    let cooldown_path = cooldown_file();
    if !cooldown_expired(&cooldown_path) {
        return HookOutput::allow();
    }

    let linear_config = load_linear_config();
    if linear_config.team_id.is_empty() {
        // No Linear config — can't file issues
        return HookOutput::allow();
    }

    // Build a compact summary for the newest actionable errors only. Prompt
    // hooks should stay small; the full error details remain in errors.jsonl.
    let error_lines: Vec<String> = errors
        .iter()
        .take(MAX_ERRORS_IN_CONTEXT)
        .map(|e| {
            format!(
                "- [{}] {}/{}: {} ({}, {})",
                e.id, e.component, e.error_type, e.error, e.severity, e.ts
            )
        })
        .collect();

    let resolutions_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("metrics")
        .join(".error-resolutions.json")
        .to_string_lossy()
        .replace('\\', "/");

    let extra = errors.len().saturating_sub(error_lines.len());
    let context = format!(
        "[Error Reporter] {} actionable infrastructure issue(s) pending.\n\
         Use Linear account \"{}\" and team \"{}\" if you are triaging infra health.\n\
         Resolution file: {}\n\
         {}\n\
         {}",
        errors.len(),
        linear_config.account,
        linear_config.team_id,
        resolutions_path,
        error_lines.join("\n"),
        if extra > 0 {
            format!("(+{} more in errors.jsonl)", extra)
        } else {
            String::new()
        }
    );

    // Write cooldown marker
    write_cooldown(&cooldown_path);

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper to create a temp errors.jsonl with given content
    fn setup_errors_file(dir: &TempDir, content: &str) -> PathBuf {
        let metrics = dir.path().join("metrics");
        fs::create_dir_all(&metrics).unwrap();
        let path = metrics.join("errors.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_read_unresolved_errors_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = setup_errors_file(&dir, "");
        let errors = read_unresolved_errors(&path);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_read_unresolved_errors_filters_resolved() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"id":"e1","component":"mcp","type":"crash","error":"timeout","severity":"warning","ts":"2026-01-01"}
{"id":"e2","component":"hook","type":"fail","error":"parse error","severity":"info","ts":"2026-01-02","resolved":"GS-100"}"#;
        let path = setup_errors_file(&dir, content);
        let errors = read_unresolved_errors(&path);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].id, "e1");
    }

    #[test]
    fn test_read_unresolved_errors_skips_malformed_lines() {
        let dir = TempDir::new().unwrap();
        let content = "not json at all\n{\"id\":\"e1\",\"component\":\"test\",\"type\":\"err\",\"error\":\"boom\",\"severity\":\"critical\",\"ts\":\"now\"}\n";
        let path = setup_errors_file(&dir, content);
        let errors = read_unresolved_errors(&path);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_cooldown_expired_no_file() {
        let path = PathBuf::from("/tmp/nonexistent-cooldown-file-test");
        assert!(cooldown_expired(&path));
    }

    #[test]
    fn test_cooldown_not_expired() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cooldown");
        fs::write(&path, now_ms().to_string()).unwrap();
        assert!(!cooldown_expired(&path));
    }

    #[test]
    fn test_cooldown_expired_old_timestamp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cooldown");
        let old = now_ms().saturating_sub(COOLDOWN_MS + 1000);
        fs::write(&path, old.to_string()).unwrap();
        assert!(cooldown_expired(&path));
    }

    #[test]
    fn test_process_no_errors_returns_allow() {
        // With no errors.jsonl file, process should return allow
        let input = HookInput::default();
        let output = process(&input);
        // We can't control the home dir in tests, but if there are no errors,
        // it should return allow (empty output)
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_error_entry_deserialization() {
        let json = r#"{"id":"e1","component":"mcp-health","type":"server_down","error":"linear timeout","severity":"warning","ts":"2026-03-01T10:00:00Z"}"#;
        let entry: ErrorEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "e1");
        assert_eq!(entry.component, "mcp-health");
        assert_eq!(entry.severity, "warning");
        assert!(entry.resolved.is_none());
    }

    #[test]
    fn test_error_entry_with_resolved_field() {
        let json = r#"{"id":"e2","component":"hook","type":"crash","error":"OOM","severity":"critical","ts":"2026-03-01","resolved":"GS-42"}"#;
        let entry: ErrorEntry = serde_json::from_str(json).unwrap();
        assert!(entry.resolved.is_some());
    }
}
