//! Channel event emitter for MCP push notifications.
//!
//! Writes event files to `~/.claude/sentinel/events/` for the sentinel-mcp
//! server to pick up and push into the Claude Code session via MCP channels.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// A lifecycle event to be pushed into the Claude Code session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEvent {
    /// Event type identifier
    pub event: String,
    /// Human-readable summary for Claude
    pub summary: String,
    /// ISO 8601 timestamp
    pub ts: String,
    /// Optional structured metadata
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

/// Get the events directory path.
pub fn events_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("events")
}

/// Emit a channel event by writing a JSON file to the events directory.
///
/// The sentinel-mcp server watches this directory and pushes each event
/// into the active Claude Code session via MCP channels.
pub fn emit(event: &str, summary: &str, meta: serde_json::Map<String, serde_json::Value>) {
    let dir = events_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
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
        meta,
    };

    let path = dir.join(&filename);
    match serde_json::to_string(&channel_event) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!(error = %e, path = %path.display(), "Failed to write channel event");
            } else {
                debug!(event, path = %path.display(), "Channel event emitted");
            }
        }
        Err(e) => warn!(error = %e, "Failed to serialize channel event"),
    }
}

/// Read and parse a channel event file.
pub fn read_event(path: &std::path::Path) -> Option<ChannelEvent> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// List all pending event files, sorted by name (oldest first).
pub fn pending_events() -> Vec<std::path::PathBuf> {
    let dir = events_dir();
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    entries
}

/// Delete an event file after it has been consumed.
pub fn consume(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        warn!(error = %e, path = %path.display(), "Failed to remove consumed event file");
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
            meta: serde_json::Map::new(),
        };

        let json = serde_json::to_string(&event).unwrap();
        std::fs::write(&path, &json).unwrap();

        let read = read_event(&path).unwrap();
        assert_eq!(read.event, "agent_completed");
        assert_eq!(read.summary, "Researcher finished");
    }
}
