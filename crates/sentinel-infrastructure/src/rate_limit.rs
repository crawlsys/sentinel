//! Per-session hook invocation rate limiting
//!
//! **Architectural hardening**: Prevents abuse via rapid hook invocation flooding.
//! Uses a sliding window counter stored as a small file per session.
//! Checked BEFORE session lock acquisition to avoid lock contention from floods.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Maximum hook invocations per window per session.
const MAX_INVOCATIONS_PER_WINDOW: usize = 120;

/// Window size in seconds (sliding window).
const WINDOW_SECONDS: u64 = 60;

/// Rate limit state file directory.
fn rate_dir() -> PathBuf {
    crate::paths::sentinel_root().join("rate")
}

fn rate_file_in_dir(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.rate"))
}

/// Check and record a hook invocation. Returns Ok(()) if allowed, Err if rate-limited.
///
/// The rate file stores newline-delimited Unix timestamps. On each call we:
/// 1. Read existing timestamps
/// 2. Prune timestamps outside the window
/// 3. Check if count exceeds the limit
/// 4. Append the current timestamp
///
/// This is intentionally not locked — a small race between concurrent hooks is
/// acceptable since the rate limit is a soft safety net, not a hard security boundary.
pub fn check_rate_limit(session_id: &str) -> Result<()> {
    check_rate_limit_in_dir(session_id, &rate_dir())
}

fn check_rate_limit_in_dir(session_id: &str, dir: &Path) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let path = rate_file_in_dir(dir, session_id);
    std::fs::create_dir_all(&dir).context("Failed to create rate limit directory")?;

    // Read existing timestamps
    let mut timestamps: Vec<u64> = if path.exists() {
        std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse::<u64>().ok())
            .collect()
    } else {
        Vec::new()
    };

    // Prune timestamps outside the window
    let cutoff = now.saturating_sub(WINDOW_SECONDS);
    timestamps.retain(|&ts| ts >= cutoff);

    // Check rate limit
    if timestamps.len() >= MAX_INVOCATIONS_PER_WINDOW {
        let _ = crate::security_log::log_security_event(
            "rate_limited",
            session_id,
            &format!(
                "Exceeded {MAX_INVOCATIONS_PER_WINDOW} hook invocations in {WINDOW_SECONDS}s — possible hook loop or abuse",
            ),
        );
        anyhow::bail!(
            "[sentinel] Rate limited: session '{session_id}' exceeded {MAX_INVOCATIONS_PER_WINDOW} hook invocations in {WINDOW_SECONDS} seconds. \
             This may indicate a hook loop or abuse.",
        );
    }

    // Record this invocation
    timestamps.push(now);

    // Write back (compact format — just timestamps)
    let content: String = timestamps
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).context("Failed to write rate limit file")?;

    Ok(())
}

/// Clean up rate limit files for sessions that no longer have state.
/// Called periodically or on session cleanup.
pub fn cleanup_stale_rate_files() -> Result<usize> {
    let dir = rate_dir();
    if !dir.exists() {
        return Ok(0);
    }

    let state_dir = crate::state_store::state_dir();
    let mut removed = 0;

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(session_id) = name.strip_suffix(".rate") {
            // If no corresponding state file exists, clean up
            let state_file = state_dir.join(format!("{session_id}.json"));
            if !state_file.exists() {
                let _ = std::fs::remove_file(entry.path());
                removed += 1;
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_allows_normal_usage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_id = "test-rate-normal";

        // First invocation should be allowed
        assert!(check_rate_limit_in_dir(session_id, tmp.path()).is_ok());

        // A few more should be fine
        for _ in 0..5 {
            assert!(check_rate_limit_in_dir(session_id, tmp.path()).is_ok());
        }
    }

    #[test]
    fn test_rate_limit_blocks_flood() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let session_id = "test-rate-flood";

        // Fill up to the limit
        for i in 0..MAX_INVOCATIONS_PER_WINDOW {
            assert!(
                check_rate_limit_in_dir(session_id, tmp.path()).is_ok(),
                "Invocation {i} should be allowed"
            );
        }

        // Next one should be blocked
        assert!(check_rate_limit_in_dir(session_id, tmp.path()).is_err());
    }
}
