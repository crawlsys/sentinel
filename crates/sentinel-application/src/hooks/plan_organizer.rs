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

/// Like [`next_versioned_path`], but tries the bare `{slug}.md` first and only
/// falls back to `-v{N}` on collision — so the common case yields a clean
/// descriptive name without a version suffix.
fn next_descriptive_path(fs: &dyn FileSystemPort, target_dir: &Path, slug: &str) -> PathBuf {
    let bare = target_dir.join(format!("{slug}.md"));
    if !fs.exists(&bare) {
        return bare;
    }
    next_versioned_path(fs, target_dir, slug)
}

/// Derive a descriptive kebab-case slug from a plan's title heading. Returns
/// `None` when the plan has no derivable title (the `plan_title_gate` makes
/// this rare for new plans, but the root-sweep can hit legacy untitled files).
///
/// "## Plan: Force Plan Organization" → `force-plan-organization`. Slugifies:
/// lowercase, every run of non-alphanumeric → single `-`, trim leading/trailing
/// `-`, cap at ~60 chars (on a `-` boundary when possible).
pub fn descriptive_slug(content: &str) -> Option<String> {
    let title = super::plan_title_gate::title_line(content)?;
    let mut slug = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        return None;
    }
    // Cap length, preferring to cut on a dash boundary.
    let capped = if slug.len() > 60 {
        let cut = slug[..60].rfind('-').unwrap_or(60);
        slug[..cut.max(1)].trim_matches('-')
    } else {
        slug
    };
    if capped.is_empty() {
        None
    } else {
        Some(capped.to_string())
    }
}

/// Whether a file's content is just a relocation pointer this hook left behind
/// (so the root-sweep doesn't try to re-process pointers).
fn is_pointer(content: &str) -> bool {
    content.trim_start().starts_with("Moved to:")
}

/// Build the pointer text left at a plan's original path after it's moved, so
/// Claude Code's `/plan` (which reads the original path) still resolves.
fn pointer_text(dest: &Path) -> String {
    format!(
        "Moved to: {}\n\nThis plan was organized by sentinel under a descriptive \
         name. Edit it at the path above.\n",
        dest.display()
    )
}

/// Move a plan's content to `dest` and replace the original at `src` with a
/// pointer. Best-effort: returns `Ok(())` only when the destination write
/// succeeds (the pointer write is non-fatal — the canonical copy is what
/// matters). `src == dest` is a no-op success.
fn move_plan(fs: &dyn FileSystemPort, src: &Path, dest: &Path, content: &str) -> anyhow::Result<()> {
    if src == dest {
        return Ok(());
    }
    fs.write(dest, content.as_bytes())?;
    // Leave a pointer at the original location (non-fatal on failure).
    if let Err(e) = fs.write(src, pointer_text(dest).as_bytes()) {
        tracing::warn!(error = %e, src = ?src, "Failed to write plan pointer (non-fatal)");
    }
    Ok(())
}

/// Sweep loose `*.md` files sitting directly in `~/.claude/plans/` into
/// per-project subfolders under descriptive names. Best-effort and bounded:
/// processes at most `MAX_SWEEP` files per run, skips pointers and dirs, never
/// deletes content (always a move-with-pointer). Legacy root files have no cwd
/// context so they land in `general/`. Returns (from → to) moves performed.
fn sweep_plan_root(fs: &dyn FileSystemPort, plans_root: &Path) -> Vec<(PathBuf, PathBuf)> {
    const MAX_SWEEP: usize = 25;
    let mut moved = Vec::new();
    let entries = match fs.read_dir(plans_root) {
        Ok(e) => e,
        Err(_) => return moved,
    };
    for path in entries {
        if moved.len() >= MAX_SWEEP {
            tracing::info!("plan root sweep hit MAX_SWEEP cap; rest left for next run");
            break;
        }
        if fs.is_dir(&path) {
            continue;
        }
        let is_md = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"));
        if !is_md {
            continue;
        }
        let content = match fs.read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if is_pointer(&content) {
            continue;
        }
        let slug = descriptive_slug(&content).unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plan")
                .to_string()
        });
        let dest_dir = plans_root.join("general");
        if fs.create_dir_all(&dest_dir).is_err() {
            continue;
        }
        let dest = next_descriptive_path(fs, &dest_dir, &slug);
        if move_plan(fs, &path, &dest, &content).is_ok() {
            moved.push((path, dest));
        }
    }
    moved
}

/// Process an `ExitPlanMode` `PostToolUse` event.
///
/// MOVES the plan from Claude Code's random-slug path into
/// `~/.claude/plans/{project}/{descriptive}.md`, leaves a pointer at the
/// original location so `/plan` still resolves, also writes the opt-in
/// repo-local `.sentinel/plans/` copy, and sweeps any loose random-slug files
/// still sitting in the plans root into project subfolders. Fully best-effort
/// and fail-open — plan organization must never block or lose a plan.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // Only fire on ExitPlanMode
    if input.tool_name.as_deref() != Some("ExitPlanMode") {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = detect_project(cwd);

    // Extract the plan file path from the tool response
    let plan_path = if let Some(p) = extract_plan_path(input.tool_result.as_ref()) {
        p
    } else {
        tracing::debug!("No plan file path in ExitPlanMode response; skipping");
        return HookOutput::allow();
    };

    let home = match ctx.fs.home_dir() {
        Some(h) => h,
        None => return HookOutput::allow(),
    };
    let plans_root = home.join(".claude").join("plans");
    let target_dir = plans_root.join(&project);

    if let Err(e) = ctx.fs.create_dir_all(&target_dir) {
        tracing::warn!(error = %e, dir = ?target_dir, "Failed to create plans dir");
        return HookOutput::allow();
    }

    // Read the plan content (from the random-slug file Claude Code just wrote).
    let plan_content = match ctx.fs.read_to_string(&plan_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, path = ?plan_path, "Failed to read plan file");
            return HookOutput::allow();
        }
    };

    // Descriptive name from the plan title; fall back to the random stem only
    // if the plan somehow has no title (the plan_title_gate makes that rare).
    let random_stem = plan_path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("plan")
        .to_string();
    let slug = descriptive_slug(&plan_content).unwrap_or_else(|| random_stem.clone());

    // MOVE the plan into {project}/{descriptive}.md and leave a pointer behind.
    let target_path = next_descriptive_path(ctx.fs, &target_dir, &slug);
    if let Err(e) = move_plan(ctx.fs, &plan_path, &target_path, &plan_content) {
        tracing::warn!(error = %e, path = ?target_path, "Failed to move plan");
        return HookOutput::allow();
    }

    tracing::info!(
        project = %project,
        slug = %slug,
        path = ?target_path,
        "Plan organized (moved to descriptive name)"
    );

    // Repo-local archive (opt-in: only when <repo>/.sentinel/plans/ exists),
    // under the descriptive slug.
    let repo_local_path =
        try_write_repo_local_archive(ctx.fs, Path::new(cwd), &slug, plan_content.as_bytes());

    // Sweep any loose random-slug files still in the plans root into folders.
    let swept = sweep_plan_root(ctx.fs, &plans_root);

    // Emit channel event for real-time notification
    let summary = format!("Plan organized: {}", target_path.display());
    let mut meta = serde_json::Map::new();
    meta.insert(
        "project".to_string(),
        serde_json::Value::String(project.clone()),
    );
    meta.insert(
        "slug".to_string(),
        serde_json::Value::String(slug.clone()),
    );
    meta.insert(
        "organized_path".to_string(),
        serde_json::Value::String(target_path.display().to_string()),
    );
    meta.insert(
        "swept_count".to_string(),
        serde_json::Value::Number(swept.len().into()),
    );
    if let Some(p) = &repo_local_path {
        meta.insert(
            "repo_local_path".to_string(),
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

    // Inject context telling the user and Claude where the plan now lives.
    let repo_local_line = match &repo_local_path {
        Some(p) => format!(
            "\nRepo-local: {} (committed with the code via .sentinel/plans/)",
            p.display()
        ),
        None => String::new(),
    };
    let swept_line = if swept.is_empty() {
        String::new()
    } else {
        format!(
            "\nAlso swept {} loose plan file(s) from the plans root into folders.",
            swept.len()
        )
    };
    let context = format!(
        "[Plan Organizer] Plan filed under a descriptive name.\n\
         \n\
         Project:  {}\n\
         Plan:     {}\n\
         Pointer:  {} (Claude Code's /plan still resolves here){}{}\n\
         \n\
         Names collide-safe: a same-named plan auto-increments to -v2, -v3, etc.",
        project,
        target_path.display(),
        plan_path.display(),
        repo_local_line,
        swept_line,
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
        fn read_to_string(&self, p: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            std::fs::read_to_string(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            std::fs::write(p, c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            std::fs::create_dir_all(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn read_dir(&self, p: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            std::fs::read_dir(p)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn append(&self, _: &Path, _: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
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
            fn read_to_string(&self, _: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
                unreachable!("find_repo_root only calls exists")
            }
            fn write(&self, _: &Path, _: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                unreachable!("find_repo_root only calls exists")
            }
            fn create_dir_all(&self, _: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                unreachable!("find_repo_root only calls exists")
            }
            fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
                unreachable!("find_repo_root only calls exists")
            }
            fn exists(&self, _: &Path) -> bool {
                false
            }
            fn is_dir(&self, _: &Path) -> bool {
                false
            }
            fn metadata(&self, _: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
                unreachable!("find_repo_root only calls exists")
            }
            fn append(&self, _: &Path, _: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
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

    // ─── descriptive naming + move + sweep ────────────────────────

    #[test]
    fn descriptive_slug_from_plan_title() {
        assert_eq!(
            descriptive_slug("## Plan: Force Plan Organization\n\nbody").as_deref(),
            Some("force-plan-organization")
        );
        assert_eq!(
            descriptive_slug("# Add User Auth (JWT)!\n").as_deref(),
            Some("add-user-auth-jwt")
        );
        // Title-less → None (sweep falls back to the file stem).
        assert_eq!(descriptive_slug("   \n\n").as_deref(), None);
    }

    #[test]
    fn descriptive_slug_caps_length() {
        let long = format!("# {}\n", "word ".repeat(40));
        let slug = descriptive_slug(&long).unwrap();
        assert!(slug.len() <= 60, "slug too long: {} ({})", slug, slug.len());
        assert!(!slug.starts_with('-') && !slug.ends_with('-'));
    }

    #[test]
    fn next_descriptive_path_prefers_bare_name() {
        let tmp = TempDir::new().unwrap();
        let p = next_descriptive_path(&RealFs, tmp.path(), "my-plan");
        assert_eq!(p, tmp.path().join("my-plan.md"));
    }

    #[test]
    fn next_descriptive_path_versions_on_collision() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("my-plan.md"), "").unwrap();
        let p = next_descriptive_path(&RealFs, tmp.path(), "my-plan");
        assert_eq!(p, tmp.path().join("my-plan-v1.md"));
    }

    #[test]
    fn move_plan_writes_dest_and_pointer() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("random-slug.md");
        let dest = tmp.path().join("sentinel").join("real-name.md");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&src, "# Real Name\nbody").unwrap();

        move_plan(&RealFs, &src, &dest, "# Real Name\nbody").unwrap();
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "# Real Name\nbody"
        );
        // Original now holds a pointer.
        let ptr = std::fs::read_to_string(&src).unwrap();
        assert!(is_pointer(&ptr), "src should be a pointer: {ptr}");
        assert!(ptr.contains("real-name.md"));
    }

    #[test]
    fn sweep_moves_loose_root_files_into_general() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // A loose random-slug plan with a title.
        std::fs::write(
            root.join("quirky-toasting-pelican.md"),
            "# OSM Fleet Build State\n\nstuff",
        )
        .unwrap();
        // A pointer file (must be skipped).
        std::fs::write(root.join("already-moved.md"), "Moved to: /elsewhere\n").unwrap();
        // A subdir (must be skipped).
        std::fs::create_dir_all(root.join("sentinel")).unwrap();

        let moved = sweep_plan_root(&RealFs, root);
        assert_eq!(moved.len(), 1, "only the titled loose file should move");
        let (from, to) = &moved[0];
        assert!(from.ends_with("quirky-toasting-pelican.md"));
        assert_eq!(to, &root.join("general").join("osm-fleet-build-state.md"));
        assert!(to.exists());
        // Original is now a pointer.
        assert!(is_pointer(
            &std::fs::read_to_string(root.join("quirky-toasting-pelican.md")).unwrap()
        ));
        // Pointer file untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("already-moved.md")).unwrap(),
            "Moved to: /elsewhere\n"
        );
    }

    #[test]
    fn sweep_falls_back_to_stem_for_titleless_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("weird-name.md"), "   \n\n").unwrap(); // no title
        let moved = sweep_plan_root(&RealFs, root);
        assert_eq!(moved.len(), 1);
        assert_eq!(
            moved[0].1,
            root.join("general").join("weird-name.md"),
            "title-less file keeps its stem"
        );
    }

    #[test]
    fn process_moves_plan_to_descriptive_name() {
        // End-to-end: a real plan file + ExitPlanMode result → moved under
        // a descriptive name, pointer left behind. Uses a scoped-home FS so
        // ~/.claude/plans/ resolves into the tempdir.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let plan_src = home.join("plan-squishy-gathering-sundae.md");
        std::fs::write(&plan_src, "## Plan: Add Caching Layer\n\nbody").unwrap();

        struct ScopedFs {
            home: PathBuf,
        }
        impl FileSystemPort for ScopedFs {
            fn home_dir(&self) -> Option<PathBuf> {
                Some(self.home.clone())
            }
            fn read_to_string(&self, p: &Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
                std::fs::read_to_string(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn write(&self, p: &Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                if let Some(par) = p.parent() {
                    std::fs::create_dir_all(par).map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
                }
                std::fs::write(p, c).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn create_dir_all(&self, p: &Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                std::fs::create_dir_all(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn read_dir(&self, p: &Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
                std::fs::read_dir(p)
                    .map_err(sentinel_domain::port_errors::FileSystemError::backend)
                    .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            }
            fn exists(&self, p: &Path) -> bool {
                p.exists()
            }
            fn is_dir(&self, p: &Path) -> bool {
                p.is_dir()
            }
            fn metadata(&self, p: &Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
                std::fs::metadata(p).map_err(sentinel_domain::port_errors::FileSystemError::backend)
            }
            fn append(&self, _: &Path, _: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
                Ok(())
            }
        }

        let fs: &'static ScopedFs = Box::leak(Box::new(ScopedFs { home: home.clone() }));
        let base = crate::hooks::test_support::stub_ctx();
        let ctx = HookContext {
            git: base.git,
            vector_store: None,
            fs,
            process: base.process,
            llm: None,
            memory_mcp: base.memory_mcp,
            env: base.env,
            linear_lookup: None,
        };
        let input = HookInput {
            tool_name: Some("ExitPlanMode".to_string()),
            cwd: Some("/Users/x/Documents/GitHub/sentinel".to_string()),
            tool_result: Some(serde_json::json!({
                "filePath": plan_src.to_string_lossy()
            })),
            ..Default::default()
        };

        let out = process(&input, &ctx);
        assert!(out.hook_specific_output.is_some(), "should inject context");
        // Plan moved to ~/.claude/plans/sentinel/add-caching-layer.md
        let dest = home
            .join(".claude")
            .join("plans")
            .join("sentinel")
            .join("add-caching-layer.md");
        assert!(dest.exists(), "plan should be moved to {dest:?}");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "## Plan: Add Caching Layer\n\nbody"
        );
        // Original is now a pointer.
        assert!(is_pointer(&std::fs::read_to_string(&plan_src).unwrap()));
    }
}
