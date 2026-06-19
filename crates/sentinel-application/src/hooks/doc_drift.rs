//! Doc Drift — Active documentation maintenance hook
//!
//! Two-phase hook following the error-reporter pattern:
//!
//! **Stop phase:** Detects when README.md, CLAUDE.md, or CHANGELOG.md are
//! stale relative to recent code changes. Writes drift findings to
//! `~/.claude/metrics/doc-drift.jsonl`.
//!
//! **`UserPromptSubmit` phase:** Reads drift findings, checks cooldown (30 min),
//! and injects explicit update instructions into Claude's context.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    concrete_input_session_id, session_path_component, EnvPort, FileSystemPort, HookContext,
};

/// Cooldown between drift reports.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_DOC_MS;

/// Docs we monitor for staleness
const MONITORED_DOCS: &[&str] = &[
    "README.md",
    "CLAUDE.md",
    "CHANGELOG.md",
    "BUILDING.md",
    "LICENSE",
    "SECURITY.md",
];

/// A single drift finding
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DriftEntry {
    doc: String,
    reason: String,
    cwd: String,
    ts: String,
    /// Set to true once Claude has acted on this drift
    #[serde(default)]
    resolved: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn metrics_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir)
}

fn drift_file(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    Some(metrics_dir(fs)?.join("doc-drift.jsonl"))
}

/// Open (or create) the sidecar lock file alongside `doc-drift.jsonl` and
/// acquire an exclusive advisory lock.
///
/// The returned `File` holds the lock for as long as it is in scope; dropping
/// it (or explicit unlock) releases the lock. This serializes the
/// read-modify-write in `resolve_drift_for_cwd` against concurrent appends
/// from `write_drift_entries`, preventing data loss when both run in parallel.
///
/// Uses a sidecar lockfile (`.lock` suffix) opened directly via `std::fs` —
/// we intentionally bypass the `FileSystemPort` abstraction here because
/// advisory file locks require a real OS `File` handle that lives across
/// the read+write operations.
fn acquire_lock(jsonl_path: &Path) -> Option<std::fs::File> {
    use fs2::FileExt;
    if let Some(parent) = jsonl_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lock_path = jsonl_path.with_extension("jsonl.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .ok()?;
    file.lock_exclusive().ok()?;
    Some(file)
}

fn env_session_id(env: &dyn EnvPort) -> Option<String> {
    env.var("CLAUDE_SESSION_ID")
        .or_else(|| env.var("SESSION_ID"))
        .and_then(|session_id| session_path_component(&session_id).map(str::to_string))
}

fn current_session_id(input: &HookInput, env: &dyn EnvPort) -> Option<String> {
    concrete_input_session_id(input)
        .map(str::to_string)
        .or_else(|| env_session_id(env))
}

fn cooldown_file(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-doc-drift-{session_id}-last"))
}

fn cooldown_expired(fs: &dyn FileSystemPort, session_id: &str) -> bool {
    let content = match fs.read_to_string(&cooldown_file(session_id)) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort, session_id: &str) {
    let _ = fs.write(&cooldown_file(session_id), now_ms().to_string().as_bytes());
}

// ---------------------------------------------------------------------------
// Stop phase: detect drift and write to doc-drift.jsonl
// ---------------------------------------------------------------------------

/// Check if a file exists and return its modification time as epoch ms.
fn file_mod_time(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

/// Get all recently modified source files (within last 5 minutes).
fn recent_source_changes(cwd: &Path) -> Vec<String> {
    let threshold = now_ms().saturating_sub(5 * 60 * 1000);
    let mut changed = Vec::new();

    let source_dirs = [
        "src",
        "lib",
        "app",
        "pages",
        "crates",
        "packages",
        "components",
    ];
    let source_exts = ["rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "cs"];

    for dir_name in &source_dirs {
        let dir = cwd.join(dir_name);
        if dir.is_dir() {
            collect_recent_files(&dir, threshold, &source_exts, &mut changed, 0, 5);
        }
    }

    // Also check root-level source files
    if let Ok(entries) = fs::read_dir(cwd) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_file()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if source_exts
                    .iter()
                    .any(|ext| name.ends_with(&format!(".{ext}")))
                {
                    if let Some(mt) = file_mod_time(&entry.path()) {
                        if mt >= threshold {
                            changed.push(name);
                        }
                    }
                }
            }
        }
    }

    changed
}

fn collect_recent_files(
    dir: &Path,
    threshold: u64,
    exts: &[&str],
    results: &mut Vec<String>,
    depth: usize,
    max_depth: usize,
) {
    if depth > max_depth {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !matches!(
                name.as_str(),
                "node_modules" | ".git" | "target" | "dist" | "build" | ".next"
            ) {
                collect_recent_files(
                    &entry.path(),
                    threshold,
                    exts,
                    results,
                    depth + 1,
                    max_depth,
                );
            }
        } else if ft.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            if exts.iter().any(|ext| name.ends_with(&format!(".{ext}"))) {
                if let Some(mt) = file_mod_time(&entry.path()) {
                    if mt >= threshold {
                        results.push(name);
                    }
                }
            }
        }
    }
}

/// Detect drift for a specific doc file.
fn check_doc_drift(cwd: &Path, doc_name: &str, recent_changes: &[String]) -> Option<DriftEntry> {
    let doc_path = cwd.join(doc_name);
    let timestamp = chrono::Utc::now().to_rfc3339();
    let cwd_str = cwd.to_string_lossy().to_string();

    match doc_name {
        "README.md" => check_readme_drift(&doc_path, recent_changes, &cwd_str, &timestamp),
        "CLAUDE.md" => check_claude_md_drift(&doc_path, cwd, &cwd_str, &timestamp),
        "CHANGELOG.md" => check_changelog_drift(&doc_path, recent_changes, &cwd_str, &timestamp),
        "BUILDING.md" | "LICENSE" | "SECURITY.md" => {
            check_standard_file_drift(&doc_path, doc_name, cwd, &cwd_str, &timestamp)
        }
        _ => None,
    }
}

fn check_readme_drift(
    path: &Path,
    recent_changes: &[String],
    cwd_str: &str,
    ts: &str,
) -> Option<DriftEntry> {
    // Missing README with source files = drift
    if !path.exists() {
        let parent = path.parent()?;
        // Only flag if there are actual source files
        let has_sources = parent.join("src").is_dir()
            || parent.join("lib").is_dir()
            || parent.join("crates").is_dir()
            || parent.join("package.json").exists()
            || parent.join("Cargo.toml").exists();
        if has_sources {
            return Some(DriftEntry {
                doc: "README.md".into(),
                reason: "Missing README.md — project has source code but no README".into(),
                cwd: cwd_str.into(),
                ts: ts.into(),
                resolved: false,
            });
        }
        return None;
    }

    let content = fs::read_to_string(path).ok()?;

    // Stub README (< 200 chars of real content)
    let stripped: String = content
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if stripped.split_whitespace().count() < 30 {
        return Some(DriftEntry {
            doc: "README.md".into(),
            reason: "README.md is a stub — less than 30 words of content".into(),
            cwd: cwd_str.into(),
            ts: ts.into(),
            resolved: false,
        });
    }

    // README not updated but significant source changes happened
    if recent_changes.len() >= 3 {
        let readme_mod = file_mod_time(path).unwrap_or(0);
        let threshold = now_ms().saturating_sub(5 * 60 * 1000);
        if readme_mod < threshold {
            return Some(DriftEntry {
                doc: "README.md".into(),
                reason: format!(
                    "README.md may be stale — {} source files changed recently but README wasn't updated",
                    recent_changes.len()
                ),
                cwd: cwd_str.into(),
                ts: ts.into(),
                resolved: false,
            });
        }
    }

    None
}

fn check_claude_md_drift(path: &Path, cwd: &Path, cwd_str: &str, ts: &str) -> Option<DriftEntry> {
    // Missing project CLAUDE.md
    if !path.exists() {
        let has_sources = cwd.join("src").is_dir()
            || cwd.join("lib").is_dir()
            || cwd.join("crates").is_dir()
            || cwd.join("package.json").exists()
            || cwd.join("Cargo.toml").exists();
        if has_sources {
            return Some(DriftEntry {
                doc: "CLAUDE.md".into(),
                reason: "Missing CLAUDE.md — project has source code but no project instructions file. Run /init or create one.".into(),
                cwd: cwd_str.into(),
                ts: ts.into(),
                resolved: false,
            });
        }
        return None;
    }

    let content = fs::read_to_string(path).ok()?;

    // Stub CLAUDE.md
    if content.len() < 200 {
        return Some(DriftEntry {
            doc: "CLAUDE.md".into(),
            reason: "CLAUDE.md is a stub — less than 200 characters".into(),
            cwd: cwd_str.into(),
            ts: ts.into(),
            resolved: false,
        });
    }

    None
}

fn check_changelog_drift(
    path: &Path,
    recent_changes: &[String],
    cwd_str: &str,
    ts: &str,
) -> Option<DriftEntry> {
    // Only flag missing changelog if there are significant recent changes
    if !path.exists() && recent_changes.len() >= 5 {
        return Some(DriftEntry {
            doc: "CHANGELOG.md".into(),
            reason: format!(
                "Missing CHANGELOG.md — {} source files changed recently, consider adding a changelog",
                recent_changes.len()
            ),
            cwd: cwd_str.into(),
            ts: ts.into(),
            resolved: false,
        });
    }

    if !path.exists() {
        return None;
    }

    let content = fs::read_to_string(path).ok()?;

    // Check if changelog has an [Unreleased] section
    if !content.contains("[Unreleased]")
        && !content.contains("Unreleased")
        && recent_changes.len() >= 3
    {
        return Some(DriftEntry {
            doc: "CHANGELOG.md".into(),
            reason: "CHANGELOG.md has no [Unreleased] section — recent changes may not be tracked"
                .into(),
            cwd: cwd_str.into(),
            ts: ts.into(),
            resolved: false,
        });
    }

    // Check if CHANGELOG is older than recently modified source files.
    // This catches committed changes where CHANGELOG wasn't updated.
    if recent_changes.len() >= 2 {
        let changelog_mtime = file_mod_time(path).unwrap_or(0);
        let threshold = now_ms().saturating_sub(5 * 60 * 1000);
        if changelog_mtime < threshold {
            return Some(DriftEntry {
                doc: "CHANGELOG.md".into(),
                reason: format!(
                    "CHANGELOG.md not updated — {} source file(s) changed recently. \
                     Add an entry under `## [Unreleased]`.",
                    recent_changes.len()
                ),
                cwd: cwd_str.into(),
                ts: ts.into(),
                resolved: false,
            });
        }
    }

    None
}

fn check_standard_file_drift(
    path: &Path,
    doc_name: &str,
    cwd: &Path,
    cwd_str: &str,
    ts: &str,
) -> Option<DriftEntry> {
    if path.exists() {
        return None;
    }

    // Only flag if this is a real project (has Cargo.toml or package.json)
    let has_sources = cwd.join("Cargo.toml").exists() || cwd.join("package.json").exists();
    if !has_sources {
        return None;
    }

    let reason = match doc_name {
        "BUILDING.md" => "Missing BUILDING.md — project has no build documentation",
        "LICENSE" => "Missing LICENSE — project has no license file",
        "SECURITY.md" => "Missing SECURITY.md — project has no security policy",
        _ => return None,
    };

    Some(DriftEntry {
        doc: doc_name.into(),
        reason: reason.into(),
        cwd: cwd_str.into(),
        ts: ts.into(),
        resolved: false,
    })
}

/// Write drift findings to doc-drift.jsonl (append mode).
fn write_drift_entries(fs_port: &dyn FileSystemPort, entries: &[DriftEntry]) {
    let path = match drift_file(fs_port) {
        Some(p) => p,
        None => return,
    };

    // Serialize against resolve_drift_for_cwd's read-modify-write.
    // The lock is held for the duration of this scope; the returned File is
    // dropped (releasing the lock) when this function returns.
    let _lock = acquire_lock(&path);

    // Read existing unresolved entries to avoid duplicates
    let existing: HashSet<String> = fs_port
        .read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<DriftEntry>(l).ok())
        .filter(|e| !e.resolved)
        .map(|e| format!("{}:{}", e.cwd, e.doc))
        .collect();

    let mut new_lines = String::new();
    for entry in entries {
        let key = format!("{}:{}", entry.cwd, entry.doc);
        if existing.contains(&key) {
            continue;
        }
        new_lines.push_str(&serde_json::to_string(entry).unwrap_or_default());
        new_lines.push('\n');
    }
    if !new_lines.is_empty() {
        let _ = fs_port.append(&path, new_lines.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: read drift findings and inject instructions
// ---------------------------------------------------------------------------

/// Read unresolved drift entries for the given cwd.
fn read_unresolved_drift(fs_port: &dyn FileSystemPort, cwd_str: &str) -> Vec<DriftEntry> {
    let path = match drift_file(fs_port) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match fs_port.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<DriftEntry>(l).ok())
        .filter(|e| !e.resolved && e.cwd == cwd_str)
        .collect()
}

/// Build context injection for doc drift findings.
fn build_drift_context(entries: &[DriftEntry]) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "[Doc Drift] {} documentation issue(s) detected in this project.",
        entries.len()
    ));
    lines.push(String::new());
    lines.push("RECOMMENDED: Update the following docs when you have a natural pause in your current task.".into());
    lines.push(String::new());

    for (i, entry) in entries.iter().enumerate() {
        lines.push(format!("{}. **{}**: {}", i + 1, entry.doc, entry.reason));

        match entry.doc.as_str() {
            "README.md" => {
                lines.push("   - If missing: create with project name, description, quick start, and architecture overview".into());
                lines.push(
                    "   - If stale: review recent changes and update relevant sections".into(),
                );
            }
            "CLAUDE.md" => {
                lines.push("   - If missing: create with project-specific instructions, key file paths, and conventions".into());
                lines.push("   - If stub: add architecture overview, key commands, and project-specific rules".into());
            }
            "CHANGELOG.md" => {
                lines.push(
                    "   - If missing: create with Keep a Changelog format and [Unreleased] section"
                        .into(),
                );
                lines.push("   - If no [Unreleased]: add one and log recent changes under appropriate categories (Added/Changed/Fixed/Removed)".into());
            }
            "BUILDING.md" | "LICENSE" | "SECURITY.md" => {
                lines.push("   - Run `sentinel init` to generate this file from templates".into());
            }
            _ => {}
        }
    }

    // Batch advice when many files are missing
    if entries.len() >= 3 {
        lines.push(String::new());
        lines.push("**TIP:** Multiple standard files are missing. Run `sentinel init --dry-run` to preview, then `sentinel init` to create them all at once.".into());
    }

    lines.push(String::new());
    lines.push(
        "After updating docs, the drift finding will clear automatically on next session.".into(),
    );

    lines.join("\n")
}

/// Mark drift entries as resolved by rewriting the file.
fn resolve_drift_for_cwd(fs_port: &dyn FileSystemPort, cwd_str: &str) {
    let path = match drift_file(fs_port) {
        Some(p) => p,
        None => return,
    };

    // Serialize the read-modify-write against concurrent appends from
    // write_drift_entries. Without this lock, Thread A's appended bytes get
    // clobbered by the final rewrite in this function.
    let _lock = acquire_lock(&path);

    let content = match fs_port.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let updated: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            if let Ok(mut entry) = serde_json::from_str::<DriftEntry>(l) {
                if entry.cwd == cwd_str && !entry.resolved {
                    entry.resolved = true;
                    return serde_json::to_string(&entry).unwrap_or_else(|_| l.to_string());
                }
            }
            l.to_string()
        })
        .collect();

    let _ = fs_port.write(&path, (updated.join("\n") + "\n").as_bytes());
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Process on Stop — detect drift and write findings.
pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd_str = input.cwd.as_deref().unwrap_or(".");
    let cwd = Path::new(cwd_str);

    if !cwd.is_dir() {
        return HookOutput::allow();
    }

    let recent_changes = recent_source_changes(cwd);

    let mut drift_entries = Vec::new();
    for doc in MONITORED_DOCS {
        if let Some(entry) = check_doc_drift(cwd, doc, &recent_changes) {
            drift_entries.push(entry);
        }
    }

    if drift_entries.is_empty() {
        // No drift detected — resolve any previous findings for this cwd
        resolve_drift_for_cwd(ctx.fs, cwd_str);
    } else {
        tracing::info!(
            count = drift_entries.len(),
            "Doc drift detected: {:?}",
            drift_entries.iter().map(|e| &e.doc).collect::<Vec<_>>()
        );
        write_drift_entries(ctx.fs, &drift_entries);
    }

    HookOutput::allow()
}

/// Process on `UserPromptSubmit` — inject update instructions if drift exists.
pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let Some(session_id) = current_session_id(input, ctx.env) else {
        tracing::warn!("doc_drift skipped prompt state without concrete session id");
        return HookOutput::allow();
    };
    let cwd_str = input.cwd.as_deref().unwrap_or(".");

    let entries = read_unresolved_drift(ctx.fs, cwd_str);
    if entries.is_empty() {
        return HookOutput::allow();
    }

    if !cooldown_expired(ctx.fs, &session_id) {
        return HookOutput::allow();
    }

    // Re-verify drift is still real (docs might have been updated since detection)
    let cwd = Path::new(cwd_str);
    let any_still_drifted = entries.iter().any(|e| {
        let doc_path = cwd.join(&e.doc);
        match e.doc.as_str() {
            "README.md" => {
                !doc_path.exists()
                    || fs::read_to_string(&doc_path)
                        .map_or(true, |c| c.split_whitespace().count() < 30)
            }
            "CLAUDE.md" => {
                !doc_path.exists() || fs::metadata(&doc_path).map_or(true, |m| m.len() < 200)
            }
            "CHANGELOG.md" | "BUILDING.md" | "LICENSE" | "SECURITY.md" => !doc_path.exists(),
            _ => false,
        }
    });

    if !any_still_drifted {
        // Drift has been resolved — clean up
        resolve_drift_for_cwd(ctx.fs, cwd_str);
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs, &session_id);

    let context = build_drift_context(&entries);
    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    #[test]
    fn test_missing_readme_with_sources() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();

        let entry = check_readme_drift(
            &dir.path().join("README.md"),
            &[],
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_some());
        assert!(entry.unwrap().reason.contains("Missing README"));
    }

    #[test]
    fn test_stub_readme() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "# Title\nShort.").unwrap();

        let entry = check_readme_drift(
            &dir.path().join("README.md"),
            &[],
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_some());
        assert!(entry.unwrap().reason.contains("stub"));
    }

    #[test]
    fn test_good_readme_no_drift() {
        let dir = tempfile::tempdir().unwrap();
        let content = "# My Project\n\nThis is a comprehensive readme with enough content to describe the project, its architecture, installation instructions, usage patterns, and contribution guidelines. It has well over thirty words of real meaningful content that describes the project.";
        fs::write(dir.path().join("README.md"), content).unwrap();

        let entry = check_readme_drift(
            &dir.path().join("README.md"),
            &[],
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_none());
    }

    #[test]
    fn test_missing_claude_md_with_sources() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();

        let entry = check_claude_md_drift(
            &dir.path().join("CLAUDE.md"),
            dir.path(),
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_some());
        assert!(entry.unwrap().reason.contains("Missing CLAUDE.md"));
    }

    #[test]
    fn test_stub_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "# Project\nTODO").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();

        let entry = check_claude_md_drift(
            &dir.path().join("CLAUDE.md"),
            dir.path(),
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_some());
        assert!(entry.unwrap().reason.contains("stub"));
    }

    #[test]
    fn test_missing_changelog_few_changes() {
        let dir = tempfile::tempdir().unwrap();

        // Only 2 changes — shouldn't flag
        let entry = check_changelog_drift(
            &dir.path().join("CHANGELOG.md"),
            &["a.rs".into(), "b.rs".into()],
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_none());
    }

    #[test]
    fn test_missing_changelog_many_changes() {
        let dir = tempfile::tempdir().unwrap();

        let changes: Vec<String> = (0..6).map(|i| format!("file{i}.rs")).collect();
        let entry = check_changelog_drift(
            &dir.path().join("CHANGELOG.md"),
            &changes,
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_some());
    }

    #[test]
    fn test_changelog_no_unreleased_section() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("CHANGELOG.md"),
            "# Changelog\n\n## [1.0.0] - 2026-01-01\n\n### Added\n- Initial release\n",
        )
        .unwrap();

        let changes: Vec<String> = (0..4).map(|i| format!("file{i}.rs")).collect();
        let entry = check_changelog_drift(
            &dir.path().join("CHANGELOG.md"),
            &changes,
            &dir.path().to_string_lossy(),
            "2026-03-05",
        );
        assert!(entry.is_some());
        assert!(entry.unwrap().reason.contains("[Unreleased]"));
    }

    #[test]
    fn test_cooldown_logic() {
        let ctx = crate::hooks::test_support::stub_ctx();
        // StubFs returns error on read → expired
        assert!(cooldown_expired(ctx.fs, "doc-drift-session"));
    }

    #[test]
    fn test_build_drift_context() {
        let entries = vec![
            DriftEntry {
                doc: "README.md".into(),
                reason: "Missing README".into(),
                cwd: "/test".into(),
                ts: "2026-03-05".into(),
                resolved: false,
            },
            DriftEntry {
                doc: "CHANGELOG.md".into(),
                reason: "No [Unreleased] section".into(),
                cwd: "/test".into(),
                ts: "2026-03-05".into(),
                resolved: false,
            },
        ];

        let ctx = build_drift_context(&entries);
        assert!(ctx.contains("[Doc Drift] 2 documentation issue(s)"));
        assert!(ctx.contains("README.md"));
        assert!(ctx.contains("CHANGELOG.md"));
        assert!(ctx.contains("Keep a Changelog"));
    }

    #[test]
    fn test_process_stop_no_dir() {
        let input = HookInput {
            cwd: Some("/nonexistent/path/that/does/not/exist".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_prompt_no_drift() {
        let dir = tempfile::tempdir().unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn missing_session_does_not_use_default_cooldown_or_inject() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let entry = DriftEntry {
            doc: "README.md".into(),
            reason: "Missing README".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            ts: "2026-05-30T00:00:00Z".into(),
            resolved: false,
        };
        let drift_path = drift_file(&fs).expect("drift path");
        std::fs::write(
            &drift_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        let default_cooldown = std::env::temp_dir().join("claude-doc-drift-default-last");
        let _ = std::fs::remove_file(&default_cooldown);

        let input = HookInput {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process_prompt(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
        assert!(!default_cooldown.exists());
    }

    #[test]
    fn concrete_input_session_writes_session_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let entry = DriftEntry {
            doc: "README.md".into(),
            reason: "Missing README".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            ts: "2026-05-30T00:00:00Z".into(),
            resolved: false,
        };
        let drift_path = drift_file(&fs).expect("drift path");
        std::fs::write(
            &drift_path,
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        let cooldown = cooldown_file("doc-drift-session");
        let _ = std::fs::remove_file(&cooldown);

        let input = HookInput {
            session_id: Some("doc-drift-session".to_string()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process_prompt(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(
            output.hook_specific_output.is_some(),
            "concrete session should receive the drift reminder"
        );
        assert!(cooldown.exists());
        let _ = std::fs::remove_file(cooldown);
    }

    #[test]
    fn test_drift_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        let drift_path = dir.path().join("doc-drift.jsonl");

        let entry = DriftEntry {
            doc: "README.md".into(),
            reason: "Missing".into(),
            cwd: "/test".into(),
            ts: "2026-03-05".into(),
            resolved: false,
        };

        // Write same entry — should deduplicate
        let line = serde_json::to_string(&entry).unwrap();
        fs::write(&drift_path, format!("{line}\n")).unwrap();

        let existing: HashSet<String> = fs::read_to_string(&drift_path)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<DriftEntry>(l).ok())
            .filter(|e| !e.resolved)
            .map(|e| format!("{}:{}", e.cwd, e.doc))
            .collect();

        assert!(existing.contains("/test:README.md"));
    }

    // -----------------------------------------------------------------------
    // Concurrency race test -- RED (expected to fail on unpatched code)
    // -----------------------------------------------------------------------
    //
    // Demonstrates the file-rewrite race between write_drift_entries (append)
    // and resolve_drift_for_cwd (read -> filter -> full-rewrite).
    //
    // Timeline of the race:
    //   Thread B (resolve): reads file  <- sees only cwd_a entries
    //   Thread A (write):   appends cwd_b entries to file
    //   Thread B (resolve): rewrites file <- clobbers cwd_b entries added by A
    //
    // The test runs 50 iterations; if any iteration loses data the assertion
    // fails -- which it does on current unpatched code.

    struct TempDirFs {
        home: std::path::PathBuf,
    }
    impl TempDirFs {
        fn new(root: &std::path::Path) -> Self {
            Self {
                home: root.to_path_buf(),
            }
        }
    }
    impl super::super::FileSystemPort for TempDirFs {
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            path: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            std::fs::read_to_string(path)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn write(
            &self,
            path: &Path,
            content: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)
                    .map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            }
            std::fs::write(path, content)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn create_dir_all(
            &self,
            path: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            std::fs::create_dir_all(path)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn read_dir(
            &self,
            path: &Path,
        ) -> Result<Vec<std::path::PathBuf>, sentinel_domain::port_errors::FileSystemError>
        {
            std::fs::read_dir(path)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }
        fn is_dir(&self, path: &Path) -> bool {
            path.is_dir()
        }
        fn metadata(
            &self,
            path: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            std::fs::metadata(path).map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
        fn append(
            &self,
            path: &Path,
            content: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            use std::io::Write as _;
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)
                    .map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)?;
            f.write_all(content)
                .map_err(sentinel_domain::port_errors::FileSystemError::backend)
        }
    }

    #[test]
    fn test_concurrent_write_and_resolve_loses_entries() {
        const ITERATIONS: usize = 50;
        let mut data_loss_seen = false;

        for iteration in 0..ITERATIONS {
            let tmp = tempfile::tempdir().unwrap();
            let metrics = tmp.path().join(".claude").join("sentinel").join("metrics");
            std::fs::create_dir_all(&metrics).unwrap();
            let drift_path = metrics.join("doc-drift.jsonl");

            let cwd_a = "/projects/alpha";
            let cwd_b = "/projects/beta";

            // Seed the file: 5 unresolved entries for cwd_a.
            // resolve_drift_for_cwd(cwd_a) will rewrite the whole file to mark
            // these resolved -- this is the window where Thread A can be clobbered.
            let seed: String = (0..5u32)
                .map(|i| {
                    let e = DriftEntry {
                        doc: format!("README{i}.md"),
                        reason: "Missing".into(),
                        cwd: cwd_a.into(),
                        ts: "2026-04-17".into(),
                        resolved: false,
                    };
                    serde_json::to_string(&e).unwrap()
                        + "
"
                })
                .collect();
            std::fs::write(&drift_path, seed).unwrap();

            // Entries Thread A will append -- different cwd so resolve should ignore them.
            let new_entries: Vec<DriftEntry> = (0..5u32)
                .map(|i| DriftEntry {
                    doc: format!("CHANGELOG{i}.md"),
                    reason: "New beta drift".into(),
                    cwd: cwd_b.into(),
                    ts: "2026-04-17".into(),
                    resolved: false,
                })
                .collect();

            use std::sync::Arc;
            let fs_a = Arc::new(TempDirFs::new(tmp.path()));
            let fs_b = Arc::new(TempDirFs::new(tmp.path()));
            let entries_clone = new_entries.clone();

            // Thread A: yield to give Thread B a head start inside the read window,
            // then append the cwd_b entries.
            let ha = std::thread::spawn(move || {
                std::thread::yield_now();
                write_drift_entries(fs_a.as_ref(), &entries_clone);
            });
            // Thread B: read -> filter -> full-rewrite for cwd_a.
            let hb = std::thread::spawn(move || {
                resolve_drift_for_cwd(fs_b.as_ref(), cwd_a);
            });
            ha.join().unwrap();
            hb.join().unwrap();

            let final_content = std::fs::read_to_string(&drift_path).unwrap_or_default();
            let survived = new_entries
                .iter()
                .filter(|exp| {
                    final_content.lines().any(|l| {
                        serde_json::from_str::<DriftEntry>(l)
                            .map(|e| e.cwd == exp.cwd && e.doc == exp.doc)
                            .unwrap_or(false)
                    })
                })
                .count();

            if survived < new_entries.len() {
                data_loss_seen = true;
                eprintln!(
                    "iteration {}: DATA LOSS -- only {}/{} cwd_b entries survived",
                    iteration,
                    survived,
                    new_entries.len()
                );
                break;
            }
        }

        // Fails on current code: the unlocked read->filter->rewrite clobbers
        // concurrent appends from write_drift_entries.
        assert!(
            !data_loss_seen,
            "BUG: resolve_drift_for_cwd clobbered entries written concurrently by              write_drift_entries -- the read->filter->rewrite is not protected by a lock"
        );
    }
}
