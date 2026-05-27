//! Plan Organizer Hook
//!
//! Fires on `PostToolUse` when `tool_name` == "`ExitPlanMode`".
//! Claude Code saves plans to `{project}/plans/{slug}.md` by default with
//! a random slug (e.g. "bright-EAGLE-river.md"). This hook archives plans
//! to TWO destinations:
//!
//! 1. **`~/.claude/plans/{project}/{slug}-v{N}.md`** — per-machine archive,
//!    always written. Cross-session backup.
//! 2. **`<repo>/.sentinel/plans/{slug}-v{N}.md`** — repo-local archive,
//!    written ONLY when the repo has been initialized with
//!    `sentinel project init` (i.e., `.sentinel/plans/` already exists).
//!    Travels with the code via git. M9.2 / task #66.
//!
//! Versions are tracked independently per destination — the global archive
//! and the repo-local archive may diverge if a plan is iterated on multiple
//! machines (the global has every iteration that machine saw; the repo-local
//! has every iteration that was committed). That's the intended behavior;
//! the two stores answer different questions.
//!
//! The original plan file is left in place so Claude Code's `/plan` command
//! can still read and edit it.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::{Path, PathBuf};

use super::{FileSystemPort, HookContext};

/// Known project directory names → plan subdirectory mappings.
/// Falls back to extracting the last path component of `cwd`.
fn detect_project(cwd: &str) -> String {
    let path = Path::new(cwd);
    let dir_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("general");

    match dir_name {
        "claude-code-marketplace" => "marketplace".to_string(),
        "firefly-pro-crm" | "firefly-pro-web-app" => "firefly-pro".to_string(),
        "sentinel" | "sentinel-launcher" => "sentinel".to_string(),
        _ => dir_name.to_string(),
    }
}

/// Extract the plan file path from the tool result JSON.
/// `ExitPlanMode` returns `{ "data": { "filePath": "...", ... } }` on success.
fn extract_plan_path(tool_result: Option<&serde_json::Value>) -> Option<PathBuf> {
    let resp = tool_result?;
    // Try direct `filePath` first
    if let Some(fp) = resp.get("filePath").and_then(|v| v.as_str()) {
        return Some(PathBuf::from(fp));
    }
    // Try nested data.filePath
    if let Some(fp) = resp
        .get("data")
        .and_then(|d| d.get("filePath"))
        .and_then(|v| v.as_str())
    {
        return Some(PathBuf::from(fp));
    }
    None
}

/// Walk up from `cwd` looking for a `.git` entry (file OR directory — a
/// worktree's `.git` is a file, the main repo's is a directory). Returns
/// the path containing `.git` (i.e. the repo root) on the first match, or
/// `None` if we hit the filesystem root without finding one.
///
/// Used to locate `<repo>/.sentinel/plans/` from inside a deep subdirectory.
/// When the hook fires from `<repo>/crates/sentinel-application/`, we still
/// want to write to `<repo>/.sentinel/plans/`, not
/// `<repo>/crates/sentinel-application/.sentinel/plans/`.
fn find_repo_root(fs: &dyn FileSystemPort, cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        if fs.exists(&current.join(".git")) {
            return Some(current);
        }
        if !current.pop() {
            // Reached filesystem root without finding .git.
            return None;
        }
    }
}

/// Find the next available version number for a given slug in the target dir.
/// Returns the path to write, e.g. `~/.claude/plans/sentinel/{slug}-v3.md`.
fn next_versioned_path(fs: &dyn FileSystemPort, target_dir: &Path, slug: &str) -> PathBuf {
    for n in 1..1000 {
        let candidate = target_dir.join(format!("{slug}-v{n}.md"));
        if !fs.exists(&candidate) {
            return candidate;
        }
    }
    // Fallback — shouldn't happen in practice
    target_dir.join(format!("{slug}-v999.md"))
}

/// Process an `ExitPlanMode` `PostToolUse` event.
/// Copies the plan file into `~/.claude/plans/{project}/{slug}-v{N}.md`.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // Only fire on ExitPlanMode
    if input.tool_name.as_deref() != Some("ExitPlanMode") {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = detect_project(cwd);

    // Extract the plan file path from the tool response
    let plan_path = if let Some(p) = extract_plan_path(input.tool_result.as_ref()) { p } else {
        tracing::debug!("No plan file path in ExitPlanMode response; skipping");
        return HookOutput::allow();
    };

    // Derive slug from filename (strip .md extension)
    let slug = plan_path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("plan")
        .to_string();

    // Build target directory
    let home = match ctx.fs.home_dir() {
        Some(h) => h,
        None => return HookOutput::allow(),
    };
    let target_dir = home.join(".claude").join("plans").join(&project);

    // Create target dir
    if let Err(e) = ctx.fs.create_dir_all(&target_dir) {
        tracing::warn!(error = %e, dir = ?target_dir, "Failed to create plans dir");
        return HookOutput::allow();
    }

    // Read the plan content
    let plan_content = match ctx.fs.read_to_string(&plan_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, path = ?plan_path, "Failed to read plan file");
            return HookOutput::allow();
        }
    };

    // Write versioned copy
    let target_path = next_versioned_path(ctx.fs, &target_dir, &slug);
    if let Err(e) = ctx.fs.write(&target_path, plan_content.as_bytes()) {
        tracing::warn!(error = %e, path = ?target_path, "Failed to write plan copy");
        return HookOutput::allow();
    }

    tracing::info!(
        project = %project,
        slug = %slug,
        path = ?target_path,
        "Plan organized (global archive)"
    );

    // M9.2 — repo-local archive. Opt-in: only writes when
    // <repo>/.sentinel/plans/ already exists (i.e. someone ran
    // `sentinel project init` in this repo). Failures here are
    // best-effort — log and continue; the global archive above is
    // the authoritative copy.
    let repo_local_path =
        try_write_repo_local_archive(ctx.fs, Path::new(cwd), &slug, plan_content.as_bytes());

    // Emit channel event for real-time notification
    let summary = format!("Plan archived: {}", target_path.display());
    let mut meta = serde_json::Map::new();
    meta.insert(
        "project".to_string(),
        serde_json::Value::String(project.clone()),
    );
    meta.insert("slug".to_string(), serde_json::Value::String(slug));
    meta.insert(
        "archived_path".to_string(),
        serde_json::Value::String(target_path.display().to_string()),
    );
    // Mention the repo-local copy in the channel event metadata too,
    // when it landed.
    if let Some(p) = &repo_local_path {
        meta.insert(
            "repo_local_archived_path".to_string(),
            serde_json::Value::String(p.display().to_string()),
        );
    }
    crate::channel_events::emit(
        ctx.fs,
        ctx.env,
        "plan_organized",
        &summary,
        meta,
        input.session_id.as_deref(),
        input.cwd.as_deref(),
        Some("plan_organizer"),
    );

    // Inject context telling the user and Claude where the archived copies live.
    let repo_local_line = match &repo_local_path {
        Some(p) => format!(
            "\nRepo-local: {} (committed with the code via .sentinel/plans/)",
            p.display()
        ),
        None => String::new(),
    };
    let context = format!(
        "[Plan Organizer] Plan archived for cross-session reference.\n\
         \n\
         Project:  {}\n\
         Original: {} (Claude Code's /plan reads from here)\n\
         Archive:  {}{}\n\
         \n\
         The archive is versioned — re-running ExitPlanMode auto-increments to -v2, -v3, etc.\n\
         The repo-local archive lands only when `.sentinel/plans/` exists \
         (run `sentinel project init` once per repo to opt in).",
        project,
        plan_path.display(),
        target_path.display(),
        repo_local_line,
    );

    HookOutput::inject_context(HookEvent::PostToolUse, context)
}

/// M9.2 — try to also archive the plan to `<repo>/.sentinel/plans/`.
///
/// Best-effort and opt-in:
/// - Returns `None` if we can't find a repo root (no `.git` above cwd).
/// - Returns `None` if `<repo>/.sentinel/plans/` doesn't exist (the repo
///   hasn't been initialized — don't create it implicitly; that's
///   `sentinel project init`'s job).
/// - Returns `None` if any FS error occurs while writing.
/// - On success, returns the path the plan landed at (versioned).
///
/// Failures here are intentionally quiet — the global archive at
/// `~/.claude/plans/...` is the authoritative copy. The repo-local one
/// is a convenience for collaboration.
fn try_write_repo_local_archive(
    fs: &dyn FileSystemPort,
    cwd: &Path,
    slug: &str,
    content: &[u8],
) -> Option<PathBuf> {
    let repo_root = find_repo_root(fs, cwd)?;
    let repo_plans = repo_root.join(".sentinel").join("plans");
    if !fs.is_dir(&repo_plans) {
        // Opt-in: only write when the user has run `sentinel project init`.
        // Don't auto-create — that would surprise users who didn't ask for
        // it.
        return None;
    }
    let target = next_versioned_path(fs, &repo_plans, slug);
    match fs.write(&target, content) {
        Ok(()) => {
            tracing::info!(path = ?target, "Plan archived (repo-local)");
            Some(target)
        }
        Err(e) => {
            tracing::warn!(error = %e, path = ?target, "Failed to write repo-local plan copy");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_ignores_non_exit_plan_mode() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_detect_project() {
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/claude-code-marketplace"),
            "marketplace"
        );
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/firefly-pro-crm"),
            "firefly-pro"
        );
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/sentinel"),
            "sentinel"
        );
        assert_eq!(
            detect_project("/home/gary/Documents/GitHub/legatus"),
            "legatus"
        );
        assert_eq!(detect_project("/"), "general");
    }

    #[test]
    fn test_extract_plan_path_direct() {
        let resp = serde_json::json!({ "filePath": "/tmp/plan.md" });
        assert_eq!(
            extract_plan_path(Some(&resp)),
            Some(PathBuf::from("/tmp/plan.md"))
        );
    }

    #[test]
    fn test_extract_plan_path_nested() {
        let resp = serde_json::json!({ "data": { "filePath": "/tmp/plan.md", "plan": "..." } });
        assert_eq!(
            extract_plan_path(Some(&resp)),
            Some(PathBuf::from("/tmp/plan.md"))
        );
    }

    #[test]
    fn test_extract_plan_path_missing() {
        let resp = serde_json::json!({ "data": { "plan": "..." } });
        assert_eq!(extract_plan_path(Some(&resp)), None);
    }

    /// Real-FS stub for next_versioned_path tests that exercise actual
    /// tempfile directories. Only needs `exists` to be accurate; defaults
    /// from FileSystemPort cover the rest.
    struct RealFs;
    impl FileSystemPort for RealFs {
        fn home_dir(&self) -> Option<PathBuf> {
            dirs::home_dir()
        }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_next_versioned_path_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let path = next_versioned_path(&RealFs, tmp.path(), "my-plan");
        assert_eq!(path, tmp.path().join("my-plan-v1.md"));
    }

    #[test]
    fn test_next_versioned_path_increments() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("my-plan-v1.md"), "").unwrap();
        std::fs::write(tmp.path().join("my-plan-v2.md"), "").unwrap();
        let path = next_versioned_path(&RealFs, tmp.path(), "my-plan");
        assert_eq!(path, tmp.path().join("my-plan-v3.md"));
    }

    #[test]
    fn test_next_versioned_path_isolated_per_slug() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("foo-v1.md"), "").unwrap();
        let path = next_versioned_path(&RealFs, tmp.path(), "bar");
        assert_eq!(path, tmp.path().join("bar-v1.md"));
    }

    // ─── M9.2 repo-local archive tests ────────────────────────────

    #[test]
    fn find_repo_root_walks_up_to_git_dir() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("my-repo");
        let deep = repo.join("crates").join("inner");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let found = find_repo_root(&RealFs, &deep);
        assert_eq!(found, Some(repo));
    }

    #[test]
    fn find_repo_root_accepts_git_file_not_just_dir() {
        // Worktrees have a `.git` FILE pointing at the main repo, not a
        // directory. find_repo_root must accept either.
        let tmp = TempDir::new().unwrap();
        let worktree = tmp.path().join("wt");
        let deep = worktree.join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: /elsewhere/.git/worktrees/wt",
        )
        .unwrap();
        let found = find_repo_root(&RealFs, &deep);
        assert_eq!(found, Some(worktree));
    }

    #[test]
    fn find_repo_root_returns_none_outside_a_repo() {
        let tmp = TempDir::new().unwrap();

        struct NoGitFs;
        impl FileSystemPort for NoGitFs {
            fn home_dir(&self) -> Option<PathBuf> {
                None
            }
            fn read_to_string(&self, _: &Path) -> anyhow::Result<String> {
                unreachable!("find_repo_root only calls exists")
            }
            fn write(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
                unreachable!("find_repo_root only calls exists")
            }
            fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> {
                unreachable!("find_repo_root only calls exists")
            }
            fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
                unreachable!("find_repo_root only calls exists")
            }
            fn exists(&self, _: &Path) -> bool {
                false
            }
            fn is_dir(&self, _: &Path) -> bool {
                false
            }
            fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> {
                unreachable!("find_repo_root only calls exists")
            }
            fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
                unreachable!("find_repo_root only calls exists")
            }
        }

        let found = find_repo_root(&NoGitFs, tmp.path());
        assert!(found.is_none());
    }

    #[test]
    fn try_write_repo_local_archive_writes_when_dir_exists() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".sentinel").join("plans")).unwrap();

        let path = try_write_repo_local_archive(&RealFs, &repo, "my-plan", b"plan content");
        assert!(path.is_some());
        let p = path.unwrap();
        assert_eq!(
            p,
            repo.join(".sentinel").join("plans").join("my-plan-v1.md")
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "plan content");
    }

    #[test]
    fn try_write_repo_local_archive_skips_when_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let path = try_write_repo_local_archive(&RealFs, &repo, "my-plan", b"plan content");
        assert!(path.is_none(), "must NOT auto-create .sentinel/plans/");
        assert!(
            !repo.join(".sentinel").exists(),
            "must not create .sentinel/ implicitly"
        );
    }

    #[test]
    fn try_write_repo_local_archive_skips_when_not_in_repo() {
        let tmp = TempDir::new().unwrap();
        let path = try_write_repo_local_archive(&RealFs, tmp.path(), "my-plan", b"plan content");
        assert!(path.is_none());
    }

    #[test]
    fn try_write_repo_local_archive_versions_independently() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let plans = repo.join(".sentinel").join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("my-plan-v1.md"), "old version").unwrap();

        let path = try_write_repo_local_archive(&RealFs, &repo, "my-plan", b"new version");
        assert_eq!(path, Some(plans.join("my-plan-v2.md")));
        assert_eq!(
            std::fs::read_to_string(plans.join("my-plan-v1.md")).unwrap(),
            "old version"
        );
    }
}
