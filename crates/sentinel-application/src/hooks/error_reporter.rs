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

/// Linear workspace config for auto-filing
struct LinearConfig {
    account: &'static str,
    team_id: &'static str,
    state_id: &'static str,
    assignee_id: &'static str,
    label_bug: &'static str,
    label_infrastructure: &'static str,
    label_auto_filed: &'static str,
}

const LINEAR_CONFIG: LinearConfig = LinearConfig {
    account: "personal",
    team_id: "2bd6c241-d8bf-4754-8b49-896525114644",
    state_id: "7a93cb67-9135-4e43-9fd8-db1ef5c4179c",
    assignee_id: "fc29de7e-51b4-494d-a117-0040d123f0a9",
    label_bug: "9b3f1e12-d2ed-46fb-8d1d-bd01be785bdb",
    label_infrastructure: "90e4b3b2-4877-436e-ad4b-9c8e1e5bb426",
    label_auto_filed: "88aee459-b906-46ca-a5c0-fda2f21df9fe",
};

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
        .collect()
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

    // Build error lines
    let error_lines: Vec<String> = errors
        .iter()
        .map(|e| {
            format!(
                "- [{}] {}/{}: {} (severity: {}, detected: {})",
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

    let context = format!(
        "[Error Reporter] {} infrastructure failure(s) detected.\n\
         \n\
         MANDATORY: Create Linear issues for these failures in Gary's personal workspace.\n\
         \n\
         Steps:\n\
         1. Switch to personal account: mcp__linear__switch_account(account_name: \"{}\")\n\
         2. For EACH error below, search first: mcp__linear__search(query: \"[Auto] {{component}}: {{brief description}}\")\n\
         3. If NO existing open issue matches, create one:\n\
            - team_id: \"{}\"\n\
            - title: \"[Auto] {{component}}/{{type}}: {{brief error description}}\"\n\
            - description: \"Auto-filed by error-reporter hook.\\n\\nError: {{full error message}}\\nSeverity: {{severity}}\\nTimestamp: {{ts}}\\nError ID: {{id}}\"\n\
            - state_id: \"{}\"\n\
            - assignee_id: \"{}\"\n\
            - label_ids: [\"{}\", \"{}\", \"{}\"]\n\
            - priority: {{use severity mapping: critical=1, warning=2, info=3}}\n\
         4. After creating ALL issues, write a resolution file:\n\
            echo '{{\"errorId1\":\"GS-123\"}}' > \"{}\"\n\
         5. Switch back to default account: mcp__linear__switch_account(account_name: \"default\")\n\
         6. Confirm to Gary: \"Filed {{N}} infrastructure issues to Linear.\"\n\
         \n\
         Errors to file:\n\
         {}",
        errors.len(),
        LINEAR_CONFIG.account,
        LINEAR_CONFIG.team_id,
        LINEAR_CONFIG.state_id,
        LINEAR_CONFIG.assignee_id,
        LINEAR_CONFIG.label_bug,
        LINEAR_CONFIG.label_infrastructure,
        LINEAR_CONFIG.label_auto_filed,
        resolutions_path,
        error_lines.join("\n"),
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
