//! IPC — Hook Client ↔ Daemon Communication
//!
//! Thin hook clients (spawned by Claude Code) talk to the daemon
//! via named pipe (Windows) or Unix socket (Linux/Mac).

use std::path::PathBuf;

/// Get the IPC socket/pipe path
pub fn ipc_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"\\.\pipe\sentinel-daemon")
    } else {
        dirs::runtime_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("sentinel.sock")
    }
}

/// Check if the daemon is running (IPC endpoint exists)
pub fn daemon_running() -> bool {
    let path = ipc_path();
    if cfg!(windows) {
        // On Windows, named pipes are virtual — try connecting
        // For now, just check if we can stat it
        path.exists() || {
            // Named pipes on Windows don't show up in filesystem
            // We'd need to try connecting
            false
        }
    } else {
        path.exists()
    }
}
