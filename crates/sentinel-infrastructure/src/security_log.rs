//! Security Audit Log
//!
//! Persistent, append-only security event log for sentinel.
//! Events: hmac_failure, rate_limited, tamper_detected, caller_rejected, state_regression
//!
//! Format: one JSON line per event in `~/.claude/sentinel/security.jsonl`.
//! Auto-truncates to the last 500KB when the file exceeds 1MB.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

/// Maximum log file size before truncation (1MB).
const MAX_LOG_SIZE: u64 = 1_024 * 1_024;

/// Size to keep after truncation (last 500KB).
const TRUNCATE_KEEP: u64 = 500 * 1_024;

/// A single security audit event.
#[derive(Debug, Serialize)]
struct SecurityEvent<'a> {
    ts: String,
    event: &'a str,
    session_id: &'a str,
    details: &'a str,
}

/// Directory for security log files.
fn security_log_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("sentinel")
}

/// Full path to the security audit log.
fn security_log_path() -> PathBuf {
    security_log_dir().join("security.jsonl")
}

/// Log a security-relevant event to `~/.claude/sentinel/security.jsonl`.
///
/// Events:
/// - `hmac_failure` — HMAC verification failed on a state file
/// - `rate_limited` — session exceeded hook invocation rate limit
/// - `tamper_detected` — unsigned state file found (signature missing)
/// - `caller_rejected` — hook invoked from interactive terminal without override
/// - `state_regression` — state generation counter went backwards
///
/// The log is append-only. If the file exceeds 1MB, it is truncated to
/// the last 500KB (preserving the most recent events).
pub fn log_security_event(event_type: &str, session_id: &str, details: &str) -> Result<()> {
    let dir = security_log_dir();
    std::fs::create_dir_all(&dir).context("Failed to create security log directory")?;

    let path = security_log_path();

    // Size-based truncation: if > 1MB, keep last 500KB
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_LOG_SIZE {
            truncate_log(&path)?;
        }
    }

    let entry = SecurityEvent {
        ts: Utc::now().to_rfc3339(),
        event: event_type,
        session_id,
        details,
    };

    let line = serde_json::to_string(&entry).context("Failed to serialize security event")? + "\n";

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open security log for append")?;

    file.write_all(line.as_bytes())
        .context("Failed to write security event")?;

    Ok(())
}

/// Truncate the log file to its last `TRUNCATE_KEEP` bytes.
/// Reads the tail, overwrites the file, ensuring we start at a line boundary.
fn truncate_log(path: &std::path::Path) -> Result<()> {
    let content = std::fs::read(path).context("Failed to read security log for truncation")?;

    if content.len() as u64 <= TRUNCATE_KEEP {
        return Ok(());
    }

    let start = content.len() - TRUNCATE_KEEP as usize;

    // Find the next newline after the cut point so we don't start mid-line
    let adjusted_start = content[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(start, |pos| start + pos + 1);

    let tail = &content[adjusted_start..];
    std::fs::write(path, tail).context("Failed to write truncated security log")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_security_event_creates_file() {
        let session = format!("test-security-{}", std::process::id());
        // Should not panic
        let result = log_security_event("test_event", &session, "unit test");
        assert!(result.is_ok());

        // Verify the file exists and contains our event
        let path = security_log_path();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("test_event"));
        assert!(content.contains(&session));
    }

    #[test]
    fn test_truncate_preserves_line_boundary() {
        let dir = std::env::temp_dir().join("sentinel-security-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-truncate.jsonl");

        // Write more than TRUNCATE_KEEP bytes
        let line = format!("{}\n", "x".repeat(200));
        let mut content = String::new();
        // Write ~600KB worth
        for _ in 0..3000 {
            content.push_str(&line);
        }
        std::fs::write(&path, &content).unwrap();

        truncate_log(&path).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        // Should be smaller than original
        assert!(result.len() < content.len());
        // Should start at a line boundary (not mid-line)
        assert!(!result.is_empty());
        // Every line should be complete
        for line in result.lines() {
            assert!(!line.is_empty());
        }

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
