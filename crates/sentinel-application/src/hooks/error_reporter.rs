//! Error Reporter Hook
//!
//! Runs on `UserPromptSubmit`. Reads `~/.claude/metrics/errors.jsonl` for
//! unresolved infrastructure errors. If any found and cooldown (10 min) has
//! expired, injects context instructing Claude to file Linear issues.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{FileSystemPort, HookContext};

/// Cooldown between error reports.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_SHORT_MS;
const MAX_ERRORS_IN_CONTEXT: usize = constants::MAX_ERRORS_IN_CONTEXT;

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
fn load_linear_config(fs: &dyn FileSystemPort) -> LinearConfig {
    let path = fs
        .claude_dir()
        .join("sentinel")
        .join("config")
        .join("error-reporter.toml");

    if let Ok(content) = fs.read_to_string(&path) {
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
        .map_or(0, |d| d.as_millis() as u64)
}

fn errors_file(fs: &dyn FileSystemPort) -> PathBuf {
    fs.claude_dir()
        .join("sentinel")
        .join("metrics")
        .join("errors.jsonl")
}

fn cooldown_file() -> PathBuf {
    std::env::temp_dir().join("claude-error-reporter-last")
}

/// Read unresolved errors from errors.jsonl
fn read_unresolved_errors(fs: &dyn FileSystemPort, path: &PathBuf) -> Vec<ErrorEntry> {
    let content = match fs.read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<ErrorEntry>(line).ok())
        .filter(|entry| entry.resolved.is_none())
        .filter(is_actionable_error)
        .collect()
}

/// Hook-level filter: well-formed entries that the domain classifier deems
/// actionable. Well-formedness (non-empty required fields) is a hook concern
/// because it depends on the JSONL DTO shape; the actionability decision
/// itself lives in `sentinel_domain::error_classifier`.
fn is_actionable_error(entry: &ErrorEntry) -> bool {
    let well_formed = !entry.id.trim().is_empty()
        && !entry.component.trim().is_empty()
        && !entry.severity.trim().is_empty()
        && !entry.error.trim().is_empty();
    well_formed && sentinel_domain::error_classifier::is_actionable_error(&entry.error)
}

/// Check if cooldown has expired
fn cooldown_expired(fs: &dyn FileSystemPort, path: &PathBuf) -> bool {
    let content = match fs.read_to_string(path) {
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
fn write_cooldown(fs: &dyn FileSystemPort, path: &PathBuf) {
    let _ = fs.write(path, now_ms().to_string().as_bytes());
}

/// Process the error-reporter hook event
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let _ = input;

    let errors_path = errors_file(ctx.fs);
    let errors = read_unresolved_errors(ctx.fs, &errors_path);

    if errors.is_empty() {
        return HookOutput::allow();
    }

    let cooldown_path = cooldown_file();
    if !cooldown_expired(ctx.fs, &cooldown_path) {
        return HookOutput::allow();
    }

    let linear_config = load_linear_config(ctx.fs);
    if linear_config.team_id.is_empty() {
        return HookOutput::allow();
    }

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

    let resolutions_path = ctx
        .fs
        .claude_dir()
        .join("sentinel")
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
            format!("(+{extra} more in errors.jsonl)")
        } else {
            String::new()
        }
    );

    write_cooldown(ctx.fs, &cooldown_path);

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test FS that reads from a HashMap — no real disk needed.
    struct MapFs {
        files: std::collections::HashMap<PathBuf, String>,
    }
    impl MapFs {
        fn new(files: Vec<(&str, &str)>) -> Self {
            Self {
                files: files
                    .into_iter()
                    .map(|(k, v)| (PathBuf::from(k), v.to_string()))
                    .collect(),
            }
        }
    }
    impl FileSystemPort for MapFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/mock/home"))
        }
        fn read_to_string(
            &self,
            p: &std::path::Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            self.files.get(p).cloned().ok_or_else(|| {
                sentinel_domain::port_errors::FileSystemError::NotFound("not found".into())
            })
        }
        fn write(
            &self,
            _: &std::path::Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
        fn create_dir_all(
            &self,
            _: &std::path::Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
        fn read_dir(
            &self,
            _: &std::path::Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, p: &std::path::Path) -> bool {
            self.files.contains_key(p)
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn metadata(
            &self,
            _: &std::path::Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::Backend(
                "no".into(),
            ))
        }
        fn append(
            &self,
            _: &std::path::Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

    #[test]
    fn test_read_unresolved_errors_empty_file() {
        let fs = MapFs::new(vec![("/test/errors.jsonl", "")]);
        let path = PathBuf::from("/test/errors.jsonl");
        let errors = read_unresolved_errors(&fs, &path);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_read_unresolved_errors_filters_resolved() {
        let content = "{\"id\":\"e1\",\"component\":\"mcp\",\"type\":\"crash\",\"error\":\"timeout\",\"severity\":\"warning\",\"ts\":\"2026-01-01\"}\n\
                       {\"id\":\"e2\",\"component\":\"hook\",\"type\":\"fail\",\"error\":\"parse error\",\"severity\":\"info\",\"ts\":\"2026-01-02\",\"resolved\":\"GS-100\"}";
        let fs = MapFs::new(vec![("/test/errors.jsonl", content)]);
        let path = PathBuf::from("/test/errors.jsonl");
        let errors = read_unresolved_errors(&fs, &path);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].id, "e1");
    }

    #[test]
    fn test_read_unresolved_errors_skips_malformed_lines() {
        let content = "not json at all\n{\"id\":\"e1\",\"component\":\"test\",\"type\":\"err\",\"error\":\"boom\",\"severity\":\"critical\",\"ts\":\"now\"}\n";
        let fs = MapFs::new(vec![("/test/errors.jsonl", content)]);
        let path = PathBuf::from("/test/errors.jsonl");
        let errors = read_unresolved_errors(&fs, &path);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_cooldown_expired_no_file() {
        let fs = MapFs::new(vec![]);
        let path = PathBuf::from("/tmp/nonexistent");
        assert!(cooldown_expired(&fs, &path));
    }

    #[test]
    fn test_cooldown_not_expired() {
        let ts = now_ms().to_string();
        let fs = MapFs::new(vec![("/tmp/cooldown", &ts)]);
        let path = PathBuf::from("/tmp/cooldown");
        assert!(!cooldown_expired(&fs, &path));
    }

    #[test]
    fn test_cooldown_expired_old_timestamp() {
        let old = now_ms().saturating_sub(COOLDOWN_MS + 1000).to_string();
        let fs = MapFs::new(vec![("/tmp/cooldown", &old)]);
        let path = PathBuf::from("/tmp/cooldown");
        assert!(cooldown_expired(&fs, &path));
    }

    #[test]
    fn test_process_no_errors_returns_allow() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_error_entry_deserialization() {
        let json = r#"{"id":"e1","component":"mcp-health","type":"server_down","error":"linear timeout","severity":"warning","ts":"2026-03-01T10:00:00Z"}"#;
        let entry: ErrorEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.id, "e1");
        assert_eq!(entry.component, "mcp-health");
        assert!(entry.resolved.is_none());
    }

    #[test]
    fn test_error_entry_with_resolved_field() {
        let json = r#"{"id":"e2","component":"hook","type":"crash","error":"OOM","severity":"critical","ts":"2026-03-01","resolved":"GS-42"}"#;
        let entry: ErrorEntry = serde_json::from_str(json).unwrap();
        assert!(entry.resolved.is_some());
    }
}
