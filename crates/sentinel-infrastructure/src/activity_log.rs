//! Activity Logger
//!
//! JSONL activity log with auto-truncation.
//! Mirrors the Node.js activity-log.js pattern.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

const MAX_LOG_SIZE: u64 = 2 * 1024 * 1024; // 2MB

/// Log directory
fn log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("logs")
}

/// Activity log entry
#[derive(Debug, Serialize)]
pub struct ActivityEntry {
    pub timestamp: String,
    pub session_id: String,
    pub event: String,
    pub hook: Option<String>,
    pub duration_ms: Option<u64>,
    pub details: Option<serde_json::Value>,
}

/// Append an activity entry
pub fn log_activity(entry: &ActivityEntry) -> Result<()> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join("activity.jsonl");

    // Auto-truncate if too large
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG_SIZE {
            truncate_log(&path)?;
        }
    }

    let line = serde_json::to_string(entry)? + "\n";
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;

    Ok(())
}

/// Log a hook invocation
pub fn log_hook(session_id: &str, hook: &str, event: &str, duration_ms: u64) -> Result<()> {
    log_activity(&ActivityEntry {
        timestamp: Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        event: event.to_string(),
        hook: Some(hook.to_string()),
        duration_ms: Some(duration_ms),
        details: None,
    })
}

/// Truncate log file to the most recent half
fn truncate_log(path: &std::path::Path) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let keep = lines.len() / 2;
    let trimmed: String = lines[lines.len() - keep..]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(path, trimmed)?;
    Ok(())
}
