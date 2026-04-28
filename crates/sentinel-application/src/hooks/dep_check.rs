//! Dependency Freshness Check — SessionStart hook
//!
//! Detects the project type from files in the working directory and runs
//! the appropriate outdated-dependency check. Results are injected into
//! session context so Claude knows about available updates.
//!
//! Supports: Rust (Cargo), Node/Bun (package.json), Python (pyproject.toml
//! / requirements.txt), Go (go.mod), Ruby (Gemfile).
//!
//! Each check has a short timeout (10s) to avoid blocking session start.
//! Results are cached per-project so subsequent sessions skip the network
//! call if the check ran recently (< 24h).

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sentinel_domain::constants::DEP_CHECK_CACHE_TTL;

use super::{FileSystemPort, HookContext, ProcessPort};

/// Detected project type and the command to check for outdated deps.
#[derive(Debug)]
enum ProjectType {
    Rust,
    Node,
    Python,
    Go,
    Ruby,
}

impl ProjectType {
    fn name(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Node => "node",
            Self::Python => "python",
            Self::Go => "go",
            Self::Ruby => "ruby",
        }
    }
}

/// Detect project type from files present in cwd.
fn detect_project_type(cwd: &Path) -> Vec<ProjectType> {
    let mut types = Vec::new();
    if cwd.join("Cargo.toml").exists() {
        types.push(ProjectType::Rust);
    }
    if cwd.join("package.json").exists() {
        types.push(ProjectType::Node);
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("requirements.txt").exists() {
        types.push(ProjectType::Python);
    }
    if cwd.join("go.mod").exists() {
        types.push(ProjectType::Go);
    }
    if cwd.join("Gemfile").exists() {
        types.push(ProjectType::Ruby);
    }
    types
}

/// Cache file path for a project's dep check results.
fn cache_path(fs: &dyn FileSystemPort, cwd: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cwd.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    Some(
        home.join(".claude")
            .join("sentinel")
            .join("state")
            .join(format!("dep-check-{hash}.txt")),
    )
}

/// Check if cached results are still fresh.
fn is_cache_fresh(fs: &dyn FileSystemPort, path: &Path) -> bool {
    if let Ok(meta) = fs.metadata(path) {
        if let Ok(modified) = meta.modified() {
            if let Ok(age) = SystemTime::now().duration_since(modified) {
                return age < DEP_CHECK_CACHE_TTL;
            }
        }
    }
    false
}

/// Run a command via `ProcessPort` and return stdout, or `None` if the
/// process failed to spawn or produced empty output.
fn run_cmd(process: &dyn ProcessPort, cmd: &str, args: &[&str], cwd: &Path) -> Option<String> {
    let cwd_str = cwd.to_str()?;
    match process.run(cmd, args, Some(cwd_str)) {
        Ok(output) => {
            if output.stdout.trim().is_empty() {
                None
            } else {
                Some(output.stdout)
            }
        }
        Err(_) => None,
    }
}

/// Check outdated deps for a given project type. Returns human-readable summary.
fn check_outdated(
    process: &dyn ProcessPort,
    project_type: &ProjectType,
    cwd: &Path,
) -> Option<String> {
    match project_type {
        ProjectType::Rust => {
            // `cargo outdated -R` requires cargo-outdated installed.
            // Fallback: `cargo update --dry-run` shows available updates.
            if let Some(output) = run_cmd(
                process,
                "cargo",
                &["outdated", "-R", "--exit-code", "1"],
                cwd,
            ) {
                Some(format!(
                    "**Rust (cargo outdated):**\n```\n{}\n```",
                    output.trim()
                ))
            } else if let Some(output) = run_cmd(process, "cargo", &["update", "--dry-run"], cwd) {
                let updates: Vec<&str> = output
                    .lines()
                    .filter(|l| l.contains("Updating") || l.contains("Adding"))
                    .collect();
                if updates.is_empty() {
                    None
                } else {
                    Some(format!(
                        "**Rust (cargo update --dry-run):**\n```\n{}\n```",
                        updates.join("\n")
                    ))
                }
            } else {
                None
            }
        }
        ProjectType::Node => {
            // Try bun first (faster), fall back to npm
            let output = run_cmd(process, "bun", &["outdated"], cwd)
                .or_else(|| run_cmd(process, "npm", &["outdated"], cwd));
            output.map(|o| format!("**Node (outdated):**\n```\n{}\n```", o.trim()))
        }
        ProjectType::Python => {
            let output = run_cmd(
                process,
                "pip",
                &["list", "--outdated", "--format=columns"],
                cwd,
            );
            output.map(|o| format!("**Python (pip outdated):**\n```\n{}\n```", o.trim()))
        }
        ProjectType::Go => {
            let output = run_cmd(process, "go", &["list", "-u", "-m", "all"], cwd);
            if let Some(ref o) = output {
                // Filter to only lines with updates (contain [v...])
                let updates: Vec<&str> = o
                    .lines()
                    .filter(|l| l.contains('[') && l.contains(']'))
                    .collect();
                if updates.is_empty() {
                    return None;
                }
                return Some(format!(
                    "**Go (outdated modules):**\n```\n{}\n```",
                    updates.join("\n")
                ));
            }
            None
        }
        ProjectType::Ruby => {
            let output = run_cmd(process, "bundle", &["outdated"], cwd);
            output.map(|o| format!("**Ruby (bundle outdated):**\n```\n{}\n```", o.trim()))
        }
    }
}

/// Process SessionStart — check for outdated dependencies.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd_str = input.cwd.as_deref().unwrap_or(".");
    let cwd = Path::new(cwd_str);

    // Check cache first
    if let Some(cache) = cache_path(ctx.fs, cwd_str) {
        if is_cache_fresh(ctx.fs, &cache) {
            // Read cached results
            if let Ok(cached) = ctx.fs.read_to_string(&cache) {
                if !cached.trim().is_empty() {
                    let context = format!(
                        "[Dep Check] Outdated dependencies detected (cached):\n\n{}",
                        cached.trim()
                    );
                    return HookOutput::inject_context(HookEvent::SessionStart, context);
                }
            }
            // Cache exists but empty — no outdated deps last time
            return HookOutput::allow();
        }
    }

    // Detect project types
    let project_types = detect_project_type(cwd);
    if project_types.is_empty() {
        return HookOutput::allow();
    }

    tracing::debug!(
        project_types = ?project_types.iter().map(|t| t.name()).collect::<Vec<_>>(),
        "Checking for outdated dependencies"
    );

    // Run checks for each detected project type
    let mut results = Vec::new();
    for pt in &project_types {
        if let Some(result) = check_outdated(ctx.process, pt, cwd) {
            results.push(result);
        }
    }

    // Cache the results (even if empty — prevents re-checking)
    if let Some(cache) = cache_path(ctx.fs, cwd_str) {
        let _ = ctx
            .fs
            .create_dir_all(cache.parent().unwrap_or(Path::new(".")));
        let _ = ctx.fs.write(&cache, results.join("\n\n").as_bytes());
    }

    if results.is_empty() {
        return HookOutput::allow();
    }

    let context = format!(
        "[Dep Check] Outdated dependencies detected:\n\n{}\n\n\
         Run the appropriate update command when you have a natural pause.",
        results.join("\n\n")
    );

    HookOutput::inject_context(HookEvent::SessionStart, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_detect_rust_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let types = detect_project_type(tmp.path());
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name(), "rust");
    }

    #[test]
    fn test_detect_node_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let types = detect_project_type(tmp.path());
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name(), "node");
    }

    #[test]
    fn test_detect_python_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "").unwrap();
        let types = detect_project_type(tmp.path());
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name(), "python");
    }

    #[test]
    fn test_detect_multi_language() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        let types = detect_project_type(tmp.path());
        assert_eq!(types.len(), 2);
    }

    #[test]
    fn test_detect_no_project() {
        let tmp = TempDir::new().unwrap();
        let types = detect_project_type(tmp.path());
        assert!(types.is_empty());
    }

    #[test]
    fn test_detect_go_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module test").unwrap();
        let types = detect_project_type(tmp.path());
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name(), "go");
    }

    #[test]
    fn test_detect_ruby_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Gemfile"), "").unwrap();
        let types = detect_project_type(tmp.path());
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name(), "ruby");
    }
}
