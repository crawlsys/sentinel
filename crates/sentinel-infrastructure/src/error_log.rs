//! Error Logger
//!
//! Collects errors with resolution tracking.
//! Mirrors the Node.js error-log.js pattern.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

const MAX_LOG_SIZE: u64 = 512 * 1024; // 512KB

fn log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("logs")
}

/// Error entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEntry {
    pub timestamp: String,
    pub session_id: String,
    pub hook: String,
    pub error: String,
    pub context: Option<serde_json::Value>,
    pub resolved: bool,
}

/// Log an error
pub fn log_error(session_id: &str, hook: &str, error: &str) -> Result<()> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join("errors.jsonl");

    // Auto-rotate
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG_SIZE {
            let archive = dir.join(format!(
                "errors-{}.jsonl",
                Utc::now().format("%Y%m%d-%H%M%S")
            ));
            std::fs::rename(&path, archive)?;
        }
    }

    let entry = ErrorEntry {
        timestamp: Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
        hook: hook.to_string(),
        error: error.to_string(),
        context: None,
        resolved: false,
    };

    let line = serde_json::to_string(&entry)? + "\n";
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;

    Ok(())
}

/// Get unresolved errors
pub fn get_unresolved() -> Result<Vec<ErrorEntry>> {
    let path = log_dir().join("errors.jsonl");
    if !path.exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)?;
    let mut errors = Vec::new();
    for line in content.lines() {
        if let Ok(entry) = serde_json::from_str::<ErrorEntry>(line) {
            if !entry.resolved {
                errors.push(entry);
            }
        }
    }
    Ok(errors)
}
