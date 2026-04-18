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

/// Check if local branch has commits not yet pushed to remote.
/// Returns false if there's no remote tracking branch (not an error).
pub fn has_unpushed_commits(repo_path: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "@{upstream}..HEAD"])
        .current_dir(repo_path)
        .output()
        .context("Failed to check unpushed commits")?;

    if !output.status.success() {
        // No upstream tracking branch — not an error, just no push target
        return Ok(false);
    }

    let count: usize = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    Ok(count > 0)
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

/// List directory basenames of all registered worktrees for this repo.
///
/// Parses `git worktree list --porcelain` and returns the trailing path segment
/// of each `worktree <path>` line. Callers compare these against directory
/// entries in `.claude/worktrees/` to distinguish orphaned directories (no
/// registry entry — truly stale) from actively-used worktrees (registered,
/// possibly in use by a parallel agent session).
///
/// Returns an empty Vec on error — callers should treat that as a signal to
/// skip the staleness check rather than assuming everything is orphaned.
pub fn list_worktree_names(repo_path: &str) -> Vec<String> {
    let Ok(output) = run_git(
        &["worktree", "list", "--porcelain"],
        repo_path,
        "Failed to list worktrees",
    ) else {
        return Vec::new();
    };

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| {
            // Take the trailing directory name — compare to entries inside
            // .claude/worktrees/ which are basenames.
            std::path::Path::new(path.trim())
                .file_name()
                .and_then(|n| n.to_str())
                .map(std::string::ToString::to_string)
        })
        .collect()
}
