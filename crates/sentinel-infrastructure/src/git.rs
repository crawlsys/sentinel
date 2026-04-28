//! Git Operations
//!
//! Wraps git CLI for status, diff, branch info.

use anyhow::{Context, Result};
use sentinel_domain::ports::GitStatusPort;
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

/// Resolve a merge-base against a base ref. Returns `None` if either ref
/// fails to resolve or the merge-base call fails (e.g. unrelated histories).
pub fn merge_base(repo_path: &str, base_ref: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["merge-base", "HEAD", base_ref])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Count commits in `<from>..HEAD`. Returns `None` if git fails or the
/// stdout doesn't parse as a non-negative integer.
pub fn rev_list_count(repo_path: &str, from: &str) -> Option<u32> {
    let output = Command::new("git")
        .args(["rev-list", "--count", &format!("{from}..HEAD")])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Run `git diff --name-only <range>` and return the changed file paths.
/// `range` may be a flag (`--cached`) or a ref-spec (`HEAD`, `main..HEAD`,
/// `<sha>..HEAD`, etc.). Returns `None` on git failure.
pub fn diff_names(repo_path: &str, range: &str) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", range])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(stdout.lines().filter(|l| !l.is_empty()).map(String::from).collect())
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

/// Return local branches fully merged into `base_ref`, excluding the base ref,
/// `main`, `master`, and the current branch marker `*`.
pub fn merged_local_branches(repo_path: &str, base_ref: &str) -> Vec<String> {
    let Ok(output) = run_git(
        &["branch", "--merged", base_ref],
        repo_path,
        "Failed to list merged local branches",
    ) else {
        return Vec::new();
    };

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(|l| l.trim_start_matches('*').trim().to_string())
        .filter(|b| {
            !b.is_empty()
                && b != base_ref
                && b != "main"
                && b != "master"
                && !b.starts_with('(')
        })
        .collect()
}

/// Return remote branches (without `origin/` prefix) fully merged into
/// `base_ref`, excluding `HEAD`, `main`, `master`, and `<base_ref>`.
pub fn merged_remote_branches(repo_path: &str, base_ref: &str) -> Vec<String> {
    let Ok(output) = run_git(
        &["branch", "-r", "--merged", base_ref],
        repo_path,
        "Failed to list merged remote branches",
    ) else {
        return Vec::new();
    };

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(|l| l.trim())
        .filter_map(|l| l.strip_prefix("origin/"))
        .map(str::trim)
        .filter(|b| {
            !b.is_empty()
                && !b.starts_with("HEAD")
                && *b != "main"
                && *b != "master"
                && *b != base_ref
        })
        .map(str::to_string)
        .collect()
}

/// Infrastructure adapter implementing `GitStatusPort`.
///
/// Delegates to the free functions above. Constructed at the composition
/// root (CLI / interceptors) and injected into hook handlers.
pub struct RealGit;

impl GitStatusPort for RealGit {
    fn has_uncommitted_changes(&self, repo_path: &str) -> Result<bool> {
        has_uncommitted_changes(repo_path)
    }

    fn changed_files(&self, repo_path: &str) -> Result<Vec<String>> {
        changed_files(repo_path)
    }

    fn current_branch(&self, repo_path: &str) -> Result<String> {
        current_branch(repo_path)
    }

    fn is_worktree(&self, repo_path: &str) -> bool {
        // Worktrees have .git as a file (pointing to the main repo), not a directory.
        std::path::Path::new(repo_path).join(".git").is_file()
    }

    fn has_unpushed_commits(&self, repo_path: &str) -> Result<bool> {
        has_unpushed_commits(repo_path)
    }

    fn repo_root(&self, path: &str) -> Option<String> {
        repo_root(path).ok()
    }

    fn list_worktree_names(&self, repo_path: &str) -> Vec<String> {
        list_worktree_names(repo_path)
    }

    fn merge_base(&self, repo_path: &str, base_ref: &str) -> Option<String> {
        merge_base(repo_path, base_ref)
    }

    fn rev_list_count(&self, repo_path: &str, from: &str) -> Option<u32> {
        rev_list_count(repo_path, from)
    }

    fn diff_names(&self, repo_path: &str, range: &str) -> Option<Vec<String>> {
        diff_names(repo_path, range)
    }

    fn merged_local_branches(&self, repo_path: &str, base_ref: &str) -> Vec<String> {
        merged_local_branches(repo_path, base_ref)
    }

    fn merged_remote_branches(&self, repo_path: &str, base_ref: &str) -> Vec<String> {
        merged_remote_branches(repo_path, base_ref)
    }
}
