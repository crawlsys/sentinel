//! Sentinel Launcher — Lightweight binary that delegates to sentinel-engine.
//!
//! This is the stable entry point (`sentinel`). It:
//! 1. Checks for a staged binary (`sentinel-engine.staged`)
//! 2. If found, verifies SHA-256 integrity against `.staged.sha256`
//! 3. Atomically swaps it into `sentinel-engine`
//! 4. Execs `sentinel-engine` with all original arguments
//!
//! The launcher itself is tiny (~200KB stripped) and never needs updating.
//! All sentinel logic lives in sentinel-engine, which can be hot-swapped
//! via `sentinel stage` without restarting Claude Code.
//!
//! **Security**: The launcher verifies the staged binary's SHA-256 hash before
//! consuming it. A staged binary without a valid `.sha256` companion file is
//! rejected and deleted. This prevents an attacker from replacing the staged
//! file with a malicious binary.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// Engine binary name — includes `.exe` on Windows, bare on Unix.
#[cfg(windows)]
const ENGINE_NAME: &str = "sentinel-engine.exe";
#[cfg(not(windows))]
const ENGINE_NAME: &str = "sentinel-engine";

fn main() -> ExitCode {
    let cargo_bin = cargo_bin_dir();
    let engine_path = cargo_bin.join(ENGINE_NAME);
    let staged_path = cargo_bin.join(format!("{ENGINE_NAME}.staged"));
    let staged_hash_path = cargo_bin.join(format!("{ENGINE_NAME}.staged.sha256"));

    // Check for staged binary and consume it
    if staged_path.exists() {
        match consume_staged(&staged_path, &staged_hash_path, &engine_path) {
            Ok(()) => {
                eprintln!("[sentinel-launcher] Consumed staged binary successfully.");
            }
            Err(e) => {
                eprintln!("[sentinel-launcher] WARNING: Failed to consume staged binary: {e}");
                // Continue with existing engine — don't block hook execution
            }
        }
    }

    // Exec sentinel-engine with all args
    if !engine_path.exists() {
        eprintln!(
            "[sentinel-launcher] FATAL: {} not found at {}",
            ENGINE_NAME,
            engine_path.display()
        );
        return ExitCode::from(1);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    match Command::new(&engine_path).args(&args).status() {
        Ok(status) => {
            if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(status.code().unwrap_or(1) as u8)
            }
        }
        Err(e) => {
            eprintln!(
                "[sentinel-launcher] FATAL: Failed to exec {}: {e}",
                engine_path.display()
            );
            ExitCode::from(1)
        }
    }
}

/// Consume a staged binary: verify integrity, swap into engine path, clean up.
fn consume_staged(staged: &PathBuf, hash_file: &PathBuf, engine: &PathBuf) -> Result<(), String> {
    // Reject staged binaries without a hash file (may be attacker-planted)
    if !hash_file.exists() {
        // Delete the unverified staged binary
        let _ = std::fs::remove_file(staged);
        return Err("SECURITY: Staged binary has no SHA-256 hash file. \
             Not staged via `sentinel stage`. Removed."
            .to_string());
    }

    let expected_hash = std::fs::read_to_string(hash_file)
        .map_err(|e| format!("Failed to read hash file: {e}"))?
        .trim()
        .to_string();

    let actual_hash =
        sha256_file(staged).map_err(|e| format!("Failed to hash staged binary: {e}"))?;

    if actual_hash != expected_hash {
        // Tampered — delete both files
        let _ = std::fs::remove_file(staged);
        let _ = std::fs::remove_file(hash_file);
        return Err(format!(
            "SECURITY: Staged binary hash mismatch! Expected: {expected_hash}, Got: {actual_hash}. \
             Files removed."
        ));
    }

    // Swap: rename staged → engine.
    //
    // On Windows, a concurrent sentinel-engine process from another Claude
    // Code session can hold `sentinel-engine.exe` open — `remove_file` then
    // fails with "Access is denied", and the staged binary sits unconsumed
    // indefinitely. Workaround: rename the locked engine out of the way
    // first (Windows allows `rename` on a running .exe even though it
    // rejects `remove_file`), then rename staged → engine. The tombstone is
    // cleaned up on the next invocation when the lock has cleared.
    if engine.exists() {
        if let Err(e) = std::fs::remove_file(engine) {
            #[cfg(windows)]
            {
                // Fallback: rename-aside, then rename staged in place.
                let tombstone = engine.with_extension("old");
                // Best-effort: clear any prior tombstone left behind.
                let _ = std::fs::remove_file(&tombstone);
                std::fs::rename(engine, &tombstone).map_err(|e2| {
                    format!(
                        "Failed to remove old engine binary ({e}) \
                         and fallback rename-aside also failed: {e2}"
                    )
                })?;
            }
            #[cfg(not(windows))]
            {
                return Err(format!("Failed to remove old engine binary: {e}"));
            }
        }
    }

    // On Windows, sweep up tombstones from previous swaps (best-effort —
    // if the old engine process is still running, this no-ops and we try
    // again next invocation).
    #[cfg(windows)]
    {
        let tombstone = engine.with_extension("old");
        if tombstone.exists() {
            let _ = std::fs::remove_file(&tombstone);
        }
    }

    std::fs::rename(staged, engine)
        .map_err(|e| format!("Failed to rename staged → engine: {e}"))?;

    // Clean up hash file
    let _ = std::fs::remove_file(hash_file);

    eprintln!(
        "[sentinel-launcher] Swapped staged binary (SHA-256: {}...)",
        &actual_hash[..16]
    );

    Ok(())
}

/// Compute SHA-256 hash of a file.
fn sha256_file(path: &PathBuf) -> Result<String, String> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;

    // Simple SHA-256 using manual implementation to avoid pulling in sha2 crate
    // in the launcher (keeping it minimal). We use the OS sha256sum instead.
    //
    // Actually, since sentinel-cli already depends on sha2, the launcher binary
    // compiled from the same crate will have it available. Let's use it directly.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    // Manual SHA-256 to avoid crate dependency (launcher should be minimal)
    // We'll shell out to certutil on Windows / sha256sum on Unix
    #[cfg(windows)]
    {
        use std::process::Command;
        let output = Command::new("certutil")
            .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
            .output()
            .map_err(|e| format!("certutil failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        // certutil output: line 1 = header, line 2 = hash, line 3 = footer
        stdout
            .lines()
            .nth(1)
            .map(|line| line.trim().replace(' ', "").to_lowercase())
            .ok_or_else(|| "Failed to parse certutil output".to_string())
    }

    #[cfg(not(windows))]
    {
        let output = std::process::Command::new("sha256sum")
            .arg(path)
            .output()
            .map_err(|e| format!("sha256sum failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
            .ok_or_else(|| "Failed to parse sha256sum output".to_string())
    }
}

/// Get the cargo bin directory (~/.cargo/bin/).
fn cargo_bin_dir() -> PathBuf {
    // Use the directory containing the launcher binary itself
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }

    // Resolve through Sentinel's home authority, but never the current working directory:
    // staged binary lookup under CWD would let an attacker-controlled project
    // directory masquerade as ~/.cargo/bin.
    sentinel_infrastructure::paths::home_root_or_fatal()
        .join(".cargo")
        .join("bin")
}
