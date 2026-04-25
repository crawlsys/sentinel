//! Plan Organizer Hook
//!
//! Fires on PostToolUse when tool_name == "ExitPlanMode".
//! Claude Code saves plans to `{project}/plans/{slug}.md` by default with
//! a random slug (e.g. "bright-EAGLE-river.md"). This hook automatically
//! copies the plan to `~/.claude/plans/{project}/{slug}-v{N}.md` with
//! auto-incrementing versions — a stable, cross-session archive.
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
/// ExitPlanMode returns `{ "data": { "filePath": "...", ... } }` on success.
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

/// Process an ExitPlanMode PostToolUse event.
/// Copies the plan file into `~/.claude/plans/{project}/{slug}-v{N}.md`.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // Only fire on ExitPlanMode
    if input.tool_name.as_deref() != Some("ExitPlanMode") {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = detect_project(cwd);

    // Extract the plan file path from the tool response
    let plan_path = match extract_plan_path(input.tool_result.as_ref()) {
        Some(p) => p,
        None => {
            tracing::debug!("No plan file path in ExitPlanMode response; skipping");
            return HookOutput::allow();
        }
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
        "Plan organized"
    );

    // Emit channel event for real-time notification
    let summary = format!("Plan archived: {}", target_path.display());
    let mut meta = serde_json::Map::new();
    meta.insert(
        "project".to_string(),
        serde_json::Value::String(project.clone()),
    );
    meta.insert("slug".to_string(), serde_json::Value::String(slug.clone()));
    meta.insert(
        "archived_path".to_string(),
        serde_json::Value::String(target_path.display().to_string()),
    );
    crate::channel_events::emit(
        ctx.fs, ctx.env,
        "plan_organized", &summary, meta,
        input.session_id.as_deref(), input.cwd.as_deref(), Some("plan_organizer"),
    );

    // Inject context telling the user and Claude where the archived copy lives
    let context = format!(
        "[Plan Organizer] Plan archived for cross-session reference.\n\
         \n\
         Project:  {}\n\
         Original: {} (Claude Code's /plan reads from here)\n\
         Archive:  {}\n\
         \n\
         The archive is versioned — re-running ExitPlanMode auto-increments to -v2, -v3, etc.",
        project,
        plan_path.display(),
        target_path.display()
    );

    HookOutput::inject_context(HookEvent::PostToolUse, context)
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
        assert_eq!(detect_project("/home/gary/Documents/GitHub/legatus"), "legatus");
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
        fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
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
            Ok(std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool { p.exists() }
        fn is_dir(&self, p: &Path) -> bool { p.is_dir() }
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
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
}
