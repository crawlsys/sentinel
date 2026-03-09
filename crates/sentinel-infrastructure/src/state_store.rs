//! State Store
//!
//! Persists session state to disk. Uses a single JSON file per session
//! instead of the 13+ temp files the Node.js hooks use.
//!
//! Uses file locking to prevent race conditions from concurrent writes.

use std::path::PathBuf;

use anyhow::{Context, Result};
use fs2::FileExt;

use sentinel_domain::state::SessionState;

/// State storage directory
fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("state")
}

/// Save session state to disk (atomic write with file lock)
///
/// Uses an exclusive lock file to serialize concurrent writes,
/// preventing corruption from parallel hook invocations.
pub fn save(state: &SessionState) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).context("Failed to create state directory")?;

    let path = dir.join(format!("{}.json", state.session_id));
    let lock_path = dir.join(format!("{}.json.lock", state.session_id));
    let tmp_path = dir.join(format!("{}.json.tmp", state.session_id));

    // Acquire exclusive lock
    let lock_file =
        std::fs::File::create(&lock_path).context("Failed to create lock file")?;
    lock_file
        .lock_exclusive()
        .context("Failed to acquire state lock")?;

    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp_path, &json).context("Failed to write temp state file")?;
    std::fs::rename(&tmp_path, &path).context("Failed to rename temp state file")?;

    // Lock released on drop
    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);

    Ok(())
}

/// Load session state from disk
pub fn load(session_id: &str) -> Result<Option<SessionState>> {
    let path = state_dir().join(format!("{session_id}.json"));
    if !path.exists() {
        return Ok(None);
    }

    let json = std::fs::read_to_string(&path).context("Failed to read state file")?;
    let state: SessionState = serde_json::from_str(&json).context("Failed to parse state")?;
    Ok(Some(state))
}

/// Delete session state
pub fn delete(session_id: &str) -> Result<()> {
    let path = state_dir().join(format!("{session_id}.json"));
    if path.exists() {
        std::fs::remove_file(&path).context("Failed to delete state file")?;
    }
    Ok(())
}

/// List all session IDs with saved state
pub fn list_sessions() -> Result<Vec<String>> {
    let dir = state_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix(".json") {
            sessions.push(id.to_string());
        }
    }
    Ok(sessions)
}
