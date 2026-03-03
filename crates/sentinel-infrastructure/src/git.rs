//! Git Operations
//!
//! Wraps git CLI for status, diff, branch info.

use anyhow::{Context, Result};
use std::process::Command;

/// Get list of changed files (staged + unstaged)
pub fn changed_files(repo_path: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git diff")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    Ok(files)
}

/// Get staged files
pub fn staged_files(repo_path: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git diff --cached")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    Ok(files)
}

/// Get current branch name
pub fn current_branch(repo_path: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_path)
        .output()
        .context("Failed to get current branch")?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check if there are uncommitted changes
pub fn has_uncommitted_changes(repo_path: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git status")?;

    Ok(!output.stdout.is_empty())
}

/// Get the repository root directory
pub fn repo_root(start_path: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start_path)
        .output()
        .context("Failed to get repo root")?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
