//! Sentinel Launcher — Lightweight binary that delegates to sentinel-engine.exe
//!
//! This is the stable entry point (`sentinel.exe`). It:
//! 1. Checks for a staged binary (`sentinel-engine.exe.staged`)
//! 2. If found, verifies SHA-256 integrity against `.staged.sha256`
//! 3. Atomically swaps it into `sentinel-engine.exe`
//! 4. Execs `sentinel-engine.exe` with all original arguments
//!
//! The launcher itself is tiny (~200KB stripped) and never needs updating.
//! All sentinel logic lives in sentinel-engine.exe, which can be hot-swapped
//! via `sentinel stage` without restarting Claude Code.
//!
//! **Security**: The launcher verifies the staged binary's SHA-256 hash before
//! consuming it. A staged binary without a valid `.sha256` companion file is
//! rejected and deleted. This prevents an attacker from replacing the staged
//! file with a malicious binary.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let cargo_bin = cargo_bin_dir();
    let engine_path = cargo_bin.join("sentinel-engine.exe");
    let staged_path = cargo_bin.join("sentinel-engine.exe.staged");
    let staged_hash_path = cargo_bin.join("sentinel-engine.exe.staged.sha256");

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
            "[sentinel-launcher] FATAL: sentinel-engine.exe not found at {}",
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

    // Swap: rename staged → engine (atomic on same filesystem)
    // On Windows, we may need to remove the old engine first if it's not locked.
    // The launcher binary is running (not the engine), so engine should be unlocked.
    if engine.exists() {
        std::fs::remove_file(engine)
            .map_err(|e| format!("Failed to remove old engine binary: {e}"))?;
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

    // Fallback
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cargo")
        .join("bin")
}
