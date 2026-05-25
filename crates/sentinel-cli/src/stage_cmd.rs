//! `sentinel stage` — Stage a new sentinel-engine binary with integrity verification
//!
//! **Attack #176 fix**: The manual `cp` workflow for staging binaries has no
//! integrity verification. An attacker with write access to `~/.cargo/bin/`
//! could replace the `.staged` file with a malicious binary.
//!
//! This command:
//! 1. Copies the new binary to `sentinel-engine.staged`
//! 2. Computes SHA-256 hash and writes to `sentinel-engine.staged.sha256`
//! 3. The hash file can be verified before consuming the staged binary
//!
//! Usage:
//!   sentinel stage                          # Uses target/release/sentinel-engine
//!   sentinel stage --binary path/to/binary  # Custom binary path
//!   sentinel verify-staged                  # Verify .staged integrity (called by launcher)

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Engine binary name — includes `.exe` on Windows, bare on Unix.
#[cfg(windows)]
const ENGINE_NAME: &str = "sentinel-engine.exe";
#[cfg(not(windows))]
const ENGINE_NAME: &str = "sentinel-engine";

/// Default binary location (relative to cwd, typically the sentinel repo root)
fn default_binary_path() -> PathBuf {
    PathBuf::from(format!("target/release/{ENGINE_NAME}"))
}

/// Path to the staged binary in ~/.cargo/bin/
fn staged_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("[sentinel] FATAL: Cannot determine home directory")?;
    Ok(home
        .join(".cargo")
        .join("bin")
        .join(format!("{ENGINE_NAME}.staged")))
}

/// Path to the staged binary's SHA-256 hash file
fn staged_hash_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("[sentinel] FATAL: Cannot determine home directory")?;
    Ok(home
        .join(".cargo")
        .join("bin")
        .join(format!("{ENGINE_NAME}.staged.sha256")))
}

/// Compute SHA-256 hash of a file
fn sha256_file(path: &std::path::Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify the staged binary's integrity against its hash file.
/// Returns Ok(hash) if valid, Err if tampered or missing.
#[allow(dead_code)]
pub fn verify_staged() -> Result<String> {
    let staged = staged_path()?;
    let hash_file = staged_hash_path()?;

    if !staged.exists() {
        anyhow::bail!("No staged binary found at {}", staged.display());
    }
    if !hash_file.exists() {
        anyhow::bail!(
            "SECURITY: Staged binary exists at {} but has no hash file. \
             This binary was NOT staged via `sentinel stage` and may be malicious. \
             Remove it manually if you trust it, or re-stage with `sentinel stage`.",
            staged.display()
        );
    }

    let expected_hash = std::fs::read_to_string(&hash_file)
        .context("Failed to read hash file")?
        .trim()
        .to_string();

    let actual_hash = sha256_file(&staged)?;

    if actual_hash != expected_hash {
        // Delete both files — the staged binary is compromised
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_file(&hash_file);
        anyhow::bail!(
            "SECURITY: Staged binary hash mismatch!\n\
             Expected: {expected_hash}\n\
             Got:      {actual_hash}\n\
             The staged binary has been tampered with and has been removed.",
        );
    }

    Ok(actual_hash)
}

pub fn run(binary: Option<String>) -> Result<()> {
    let source = binary.map_or_else(default_binary_path, PathBuf::from);

    if !source.exists() {
        anyhow::bail!(
            "Binary not found at {}. Build first with `cargo build --release -p sentinel`",
            source.display()
        );
    }

    let staged = staged_path()?;
    let hash_file = staged_hash_path()?;

    // Compute hash of source binary BEFORE copying
    let hash = sha256_file(&source)?;

    // Copy binary to staged location
    std::fs::copy(&source, &staged)
        .with_context(|| format!("Failed to copy {} → {}", source.display(), staged.display()))?;

    // Write hash file
    std::fs::write(&hash_file, format!("{hash}\n"))
        .with_context(|| format!("Failed to write hash file {}", hash_file.display()))?;

    // Verify the copy matches
    let verify_hash = sha256_file(&staged)?;
    if verify_hash != hash {
        let _ = std::fs::remove_file(&staged);
        let _ = std::fs::remove_file(&hash_file);
        anyhow::bail!(
            "Staged binary hash doesn't match source after copy! \
             Source: {hash}, Staged: {verify_hash}. Files removed."
        );
    }

    let size = std::fs::metadata(&staged)?.len();
    eprintln!(
        "[sentinel] Staged binary: {} ({} bytes, SHA-256: {})",
        staged.display(),
        size,
        &hash[..16],
    );
    eprintln!("[sentinel] Hash file: {}", hash_file.display());
    eprintln!("[sentinel] Next hook invocation will consume the staged binary.");

    Ok(())
}
