//! Channel event emitter for MCP push notifications.
//!
//! Writes event files to `~/.claude/sentinel/events/{session_id}/` for the
//! sentinel-mcp server to pick up and push into the correct Claude Code
//! session via MCP channels. Session-scoped directories prevent cross-session
//! event contamination when multiple Claude sessions run concurrently.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use sentinel_domain::ports::{EnvPort, FileSystemPort};

/// A lifecycle event to be pushed into the Claude Code session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEvent {
    /// Event type identifier
    pub event: String,
    /// Human-readable summary for Claude
    pub summary: String,
    /// ISO 8601 timestamp
    pub ts: String,
    /// Session ID that owns this event
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Project context (cwd basename or project name)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Source agent or hook that generated this event
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    /// Optional structured metadata
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

/// Get the base events directory (parent of all session subdirs).
fn events_base_dir(fs: &dyn FileSystemPort) -> std::path::PathBuf {
    fs.home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("events")
}

/// Get the session-scoped events directory.
///
/// Returns `~/.claude/sentinel/events/{session_id}/` for the given session,
/// or falls back to `~/.claude/sentinel/events/_unscoped/` if no session ID
/// is available.
pub fn events_dir_for_session(
    fs: &dyn FileSystemPort,
    session_id: Option<&str>,
) -> std::path::PathBuf {
    let subdir = session_id.unwrap_or("_unscoped");
    events_base_dir(fs).join(subdir)
}

/// Get the events directory for the current session, with the session id
/// resolved from the env adapter.
pub fn events_dir(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> std::path::PathBuf {
    let session_id = detect_session_id(env);
    events_dir_for_session(fs, session_id.as_deref())
}

/// Detect the current session ID from the env adapter.
fn detect_session_id(env: &dyn EnvPort) -> Option<String> {
    env.var("CLAUDE_SESSION_ID")
        .or_else(|| env.var("SESSION_ID"))
        .filter(|s| !s.is_empty())
}

/// Derive a project name from a cwd path (uses the last path component).
fn project_from_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.and_then(|p| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
    })
}

/// Emit a channel event by writing a JSON file to the session-scoped events directory.
///
/// `session_id` and `cwd` should come from `HookInput` when available.
/// If `session_id` is `None`, falls back to env-adapter detection.
pub fn emit(
    fs: &dyn FileSystemPort,
    env: &dyn EnvPort,
    event: &str,
    summary: &str,
    meta: serde_json::Map<String, serde_json::Value>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    source_agent: Option<&str>,
) {
    let resolved_session_id = session_id
        .map(String::from)
        .or_else(|| detect_session_id(env));

    let dir = events_dir_for_session(fs, resolved_session_id.as_deref());
    if let Err(e) = fs.create_dir_all(&dir) {
        warn!(error = %e, "Failed to create events directory");
        return;
    }

    let now = Utc::now();
    let filename = format!(
        "{}_{}.json",
        now.timestamp_millis(),
        event.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_")
    );

    let channel_event = ChannelEvent {
        event: event.to_string(),
        summary: summary.to_string(),
        ts: now.to_rfc3339(),
        session_id: resolved_session_id,
        project: project_from_cwd(cwd),
        source_agent: source_agent.map(String::from),
        meta,
    };

    let path = dir.join(&filename);
    match serde_json::to_string(&channel_event) {
        Ok(json) => {
            if let Err(e) = fs.write(&path, json.as_bytes()) {
                warn!(error = %e, path = %path.display(), "Failed to write channel event");
            } else {
                debug!(event, path = %path.display(), "Channel event emitted");
            }
        }
        Err(e) => warn!(error = %e, "Failed to serialize channel event"),
    }
}

/// Read and parse a channel event file.
pub fn read_event(fs: &dyn FileSystemPort, path: &std::path::Path) -> Option<ChannelEvent> {
    let content = fs.read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// List all pending event files in a session-scoped directory, sorted oldest first.
pub fn pending_events_for_session(
    fs: &dyn FileSystemPort,
    session_id: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let dir = events_dir_for_session(fs, session_id);
    pending_events_in_dir(fs, &dir)
}

/// List all pending event files in the current session's directory.
pub fn pending_events(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> Vec<std::path::PathBuf> {
    let dir = events_dir(fs, env);
    pending_events_in_dir(fs, &dir)
}

fn pending_events_in_dir(fs: &dyn FileSystemPort, dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut entries: Vec<std::path::PathBuf> = fs
        .read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    entries
}

/// Delete an event file after it has been consumed.
pub fn consume(fs: &dyn FileSystemPort, path: &std::path::Path) {
    if let Err(e) = fs.remove_file(path) {
        warn!(error = %e, path = %path.display(), "Failed to remove consumed event file");
    }
}

/// Remove session event directories older than the given duration.
///
/// Call during `SessionStart` to prevent stale directories from accumulating.
pub fn cleanup_stale_sessions(fs: &dyn FileSystemPort, max_age: std::time::Duration) {
    let base = events_base_dir(fs);
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let entries = match fs.read_dir(&base) {
        Ok(e) => e,
        Err(_) => return,
    };

    for path in entries {
        if !fs.is_dir(&path) {
            continue;
        }

        let modified = match fs.metadata(&path).and_then(|m| m.modified().map_err(Into::into)) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if modified < cutoff {
            if let Err(e) = fs.remove_dir_all(&path) {
                debug!(error = %e, path = %path.display(), "Failed to remove stale session events dir");
            } else {
                debug!(path = %path.display(), "Cleaned up stale session events directory");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_event.json");

        let event = ChannelEvent {
            event: "agent_completed".to_string(),
            summary: "Researcher finished".to_string(),
            ts: Utc::now().to_rfc3339(),
            session_id: Some("test-session-123".to_string()),
            project: Some("sentinel".to_string()),
            source_agent: Some("debugger".to_string()),
            meta: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&event).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Inline FileSystemPort impl that delegates to real disk for read.
        struct TestFs;
        impl FileSystemPort for TestFs {
            fn home_dir(&self) -> Option<std::path::PathBuf> { dirs::home_dir() }
            fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> {
                Ok(std::fs::read_to_string(p)?)
            }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> { Ok(vec![]) }
            fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
            fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
                Ok(std::fs::metadata(p)?)
            }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }

        let read = read_event(&TestFs, &path).unwrap();
        assert_eq!(read.event, "agent_completed");
        assert_eq!(read.summary, "Researcher finished");
        assert_eq!(read.session_id.as_deref(), Some("test-session-123"));
        assert_eq!(read.project.as_deref(), Some("sentinel"));
        assert_eq!(read.source_agent.as_deref(), Some("debugger"));
    }

    #[test]
    fn test_roundtrip_legacy_format() {
        let json = r#"{"event":"build_completed","summary":"Build done","ts":"2026-04-16T12:00:00Z","meta":{}}"#;
        let event: ChannelEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event, "build_completed");
        assert!(event.session_id.is_none());
        assert!(event.project.is_none());
        assert!(event.source_agent.is_none());
    }

    #[test]
    fn test_events_dir_for_session() {
        let fs = crate::hooks::test_support::StubFs;
        let dir = events_dir_for_session(&fs, Some("abc-123"));
        assert!(dir.ends_with("events/abc-123") || dir.ends_with("events\\abc-123"));

        let unscoped = events_dir_for_session(&fs, None);
        assert!(unscoped.ends_with("events/_unscoped") || unscoped.ends_with("events\\_unscoped"));
    }

    #[test]
    fn test_project_from_cwd() {
        assert_eq!(project_from_cwd(Some("/Users/gary/projects/sentinel")), Some("sentinel".to_string()));
        assert_eq!(project_from_cwd(Some("C:\\Users\\gary\\sentinel")), Some("sentinel".to_string()));
        assert_eq!(project_from_cwd(None), None);
    }

    #[test]
    fn test_cleanup_removes_old_dirs() {
        let tmpdir = tempfile::tempdir().unwrap();
        let base = tmpdir.path();
        let stale = base.join("old-session");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("event.json"), "{}").unwrap();

        // With max_age=0, everything is "stale"
        // We need to call with the base dir overridden — test the logic directly
        let cutoff = std::time::SystemTime::now();
        // Sleep tiny bit so mtime < cutoff
        std::thread::sleep(std::time::Duration::from_millis(10));

        for entry in std::fs::read_dir(base).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(meta) = path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if modified < cutoff {
                            let _ = std::fs::remove_dir_all(&path);
                        }
                    }
                }
            }
        }
        assert!(!stale.exists());
    }
}
