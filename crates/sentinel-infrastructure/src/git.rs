//! Git Operations
//!
//! Wraps git CLI for status, diff, branch info.

use anyhow::{Context, Result};
use std::process::Command;

/// Run a git command and check exit status
fn run_git(args: &[&str], dir: &str, description: &str) -> Result<std::process::Output> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| description.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}: {}", description, stderr.trim());
    }

    Ok(output)
}

/// Get list of changed files (staged + unstaged)
pub fn changed_files(repo_path: &str) -> Result<Vec<String>> {
    let output = run_git(
        &["diff", "--name-only", "HEAD"],
        repo_path,
        "Failed to run git diff",
    )?;

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
    let output = run_git(
        &["diff", "--cached", "--name-only"],
        repo_path,
        "Failed to run git diff --cached",
    )?;

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
    let output = run_git(
        &["rev-parse", "--abbrev-ref", "HEAD"],
        repo_path,
        "Failed to get current branch",
    )?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check if there are uncommitted changes
pub fn has_uncommitted_changes(repo_path: &str) -> Result<bool> {
    let output = run_git(
        &["status", "--porcelain"],
        repo_path,
        "Failed to run git status",
    )?;

    Ok(!output.stdout.is_empty())
}

/// Get the repository root directory
pub fn repo_root(start_path: &str) -> Result<String> {
    let output = run_git(
        &["rev-parse", "--show-toplevel"],
        start_path,
        "Failed to get repo root",
    )?;

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
