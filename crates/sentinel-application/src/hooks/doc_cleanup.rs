//! Doc Cleanup — Two-phase hook
//!
//! **Stop phase:** Scans for junk `.md` files (empty/stub, TODO-only,
//! orphaned root-level docs). Writes findings to
//! `~/.claude/metrics/doc-cleanup.json`.
//!
//! **`UserPromptSubmit` phase:** Reads findings, checks cooldown (30 min),
//! injects cleanup instructions.

use regex::Regex;
use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{FileSystemPort, HookContext};

/// Cooldown between cleanup reminders.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_DOC_MS;

/// Root-level `.md` files that are expected and not considered orphaned.
const ALLOWED_ROOT_MD: &[&str] = &[
    "README.md",
    "CHANGELOG.md",
    "CONTRIBUTING.md",
    "LICENSE.md",
    "CODE_OF_CONDUCT.md",
    "SECURITY.md",
    "CLAUDE.md",
    "todos.md",
];

/// Directories to skip during the scan.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".next",
    ".svelte-kit",
    "__pycache__",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct JunkDoc {
    path: String,
    reason: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CleanupState {
    cwd: String,
    junk_docs: Vec<JunkDoc>,
    ts: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn state_file(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join("doc-cleanup.json"))
}

fn cooldown_file() -> PathBuf {
    std::env::temp_dir().join("claude-doc-cleanup-last")
}

fn cooldown_expired(fs: &dyn FileSystemPort) -> bool {
    let content = match fs.read_to_string(&cooldown_file()) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort) {
    let _ = fs.write(&cooldown_file(), now_ms().to_string().as_bytes());
}

/// Recursively scan `dir` for junk `.md` files up to `max_depth`.
fn scan_docs(
    fs: &dyn FileSystemPort,
    dir: &Path,
    cwd: &Path,
    depth: usize,
    max_depth: usize,
    results: &mut Vec<JunkDoc>,
) {
    if depth > max_depth {
        return;
    }

    let entries = match fs.read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let todo_re =
        Regex::new(r"(?i)^#.*\n+\s*(TODO|TBD|Coming soon|Add content)").expect("valid regex");

    for path in entries {
        let name_buf = match path.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let name_str = name_buf.to_string_lossy();

        if fs.is_dir(&path) {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            scan_docs(fs, &path, cwd, depth + 1, max_depth, results);
            continue;
        }

        if !name_str.ends_with(".md") {
            continue;
        }

        let content = match fs.read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relative = path
            .strip_prefix(cwd)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Strip headings and whitespace to measure actual content length
        let stripped = content
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join(" ");
        let stripped = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

        if stripped.len() < 100 {
            results.push(JunkDoc {
                path: relative,
                reason: "empty/stub (less than 100 chars of content)".into(),
            });
            continue;
        }

        if todo_re.is_match(&content) {
            results.push(JunkDoc {
                path: relative,
                reason: "TODO-only placeholder".into(),
            });
            continue;
        }

        // Orphaned: non-allowed .md in the root directory, small content
        if depth == 0 && !ALLOWED_ROOT_MD.contains(&name_str.as_ref()) && stripped.len() < 200 {
            results.push(JunkDoc {
                path: relative,
                reason: "orphaned in root (should be in docs/ or deleted)".into(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Stop phase: detect junk docs and write state
// ---------------------------------------------------------------------------

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd_str = input.cwd.as_deref().unwrap_or(".");
    let cwd = Path::new(cwd_str);

    if !cwd.is_dir() {
        return HookOutput::allow();
    }

    let mut results: Vec<JunkDoc> = Vec::new();
    scan_docs(ctx.fs, cwd, cwd, 0, 3, &mut results);

    if results.is_empty() {
        // No junk — clear any previous state
        if let Some(path) = state_file(ctx.fs) {
            let _ = ctx.fs.write(&path, b"");
        }
        return HookOutput::allow();
    }

    let state = CleanupState {
        cwd: cwd_str.to_string(),
        junk_docs: results,
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = state_file(ctx.fs) {
        let _ = ctx.fs.write(
            &path,
            serde_json::to_string(&state).unwrap_or_default().as_bytes(),
        );
    }

    tracing::info!(count = state.junk_docs.len(), "Junk docs detected");

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject cleanup instructions
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let path = match state_file(ctx.fs) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: CleanupState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Only inject for the current project
    if state.cwd != cwd {
        return HookOutput::allow();
    }

    if !cooldown_expired(ctx.fs) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs);

    let doc_list: String = state
        .junk_docs
        .iter()
        .take(8)
        .map(|d| format!("  - `{}` — {}", d.path, d.reason))
        .collect::<Vec<_>>()
        .join("\n");

    let extra = if state.junk_docs.len() > 8 {
        format!("\n  ... and {} more", state.junk_docs.len() - 8)
    } else {
        String::new()
    };

    let context = format!(
        "[Doc Cleanup] {} junk documentation file(s) found in this project.\n\
         When you have a natural pause, clean these up:\n\
         {doc_list}{extra}\n\
         \n\
         Actions: delete empty stubs, flesh out TODO-only files, or move orphaned docs into `docs/`.",
        state.junk_docs.len(),
    );

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Real-FS port impl for tests that exercise actual tempfile directories.
    struct RealTestFs;
    impl FileSystemPort for RealTestFs {
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
    fn test_empty_dir_no_junk() {
        let dir = tempfile::tempdir().unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allowed_root_md_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("README.md"),
            "# README\n\nThis is a proper readme with enough content to exceed the threshold for empty detection. It has multiple sentences and paragraphs of real content.",
        )
        .unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_empty_md_detected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.md"), "# Notes\n\nTODO").unwrap();
        let mut results = Vec::new();
        scan_docs(&RealTestFs, dir.path(), dir.path(), 0, 3, &mut results);
        assert!(!results.is_empty());
        assert!(results[0].reason.contains("empty/stub"));
    }

    #[test]
    fn test_todo_only_detected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("feature.md"),
            "# Feature\n\nTODO: implement this feature with all the details and make sure it covers enough content to be over 100 characters of stripped content",
        )
        .unwrap();
        let mut results = Vec::new();
        scan_docs(&RealTestFs, dir.path(), dir.path(), 0, 3, &mut results);
        assert!(!results.is_empty());
        assert!(results[0].reason.contains("TODO-only"));
    }

    #[test]
    fn test_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let nm = dir.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("junk.md"), "").unwrap();
        let mut results = Vec::new();
        scan_docs(&RealTestFs, dir.path(), dir.path(), 0, 3, &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn test_scan_docs_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b").join("c").join("d");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.md"), "").unwrap();
        let mut results = Vec::new();
        scan_docs(&RealTestFs, dir.path(), dir.path(), 0, 3, &mut results);
        assert!(results.is_empty());
    }

    #[test]
    fn test_prompt_no_state_returns_allow() {
        let input = HookInput {
            cwd: Some("/nonexistent/test/path".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_cooldown_logic() {
        let ctx = crate::hooks::test_support::stub_ctx();
        // StubFs returns error on read → expired
        assert!(cooldown_expired(ctx.fs));
    }
}
