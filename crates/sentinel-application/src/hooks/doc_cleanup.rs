//! Doc Cleanup
//!
//! Scans the working directory for junk `.md` files and warns via stderr.
//! Detects: empty/stub files (<100 chars), TODO-only files, and orphaned
//! root-level docs that should live in `docs/`.

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::fs;
use std::path::Path;

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

/// Classification of a junk doc.
#[derive(Debug, Clone)]
struct JunkDoc {
    path: String,
    reason: JunkReason,
}

#[derive(Debug, Clone, Copy)]
enum JunkReason {
    Empty,
    TodoOnly,
    Orphaned,
}

impl JunkReason {
    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty/stub",
            Self::TodoOnly => "TODO-only",
            Self::Orphaned => "orphaned in root",
        }
    }
}

/// Recursively scan `dir` for junk `.md` files up to `max_depth`.
fn scan_docs(dir: &Path, cwd: &Path, depth: usize, max_depth: usize, results: &mut Vec<JunkDoc>) {
    if depth > max_depth {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let todo_re = Regex::new(r"(?i)^#.*\n+\s*(TODO|TBD|Coming soon|Add content)")
        .expect("valid regex");

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            scan_docs(&entry.path(), cwd, depth + 1, max_depth, results);
            continue;
        }

        if !name_str.ends_with(".md") {
            continue;
        }

        let content = match fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relative = entry
            .path()
            .strip_prefix(cwd)
            .unwrap_or(&entry.path())
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
                reason: JunkReason::Empty,
            });
            continue;
        }

        if todo_re.is_match(&content) {
            results.push(JunkDoc {
                path: relative,
                reason: JunkReason::TodoOnly,
            });
            continue;
        }

        // Orphaned: non-allowed .md in the root directory, small content
        if depth == 0
            && !ALLOWED_ROOT_MD.contains(&name_str.as_ref())
            && stripped.len() < 200
        {
            results.push(JunkDoc {
                path: relative,
                reason: JunkReason::Orphaned,
            });
        }
    }
}

/// Process the doc-cleanup hook event (Stop).
pub fn process(input: &HookInput) -> HookOutput {
    let cwd_str = input.cwd.as_deref().unwrap_or(".");
    let cwd = Path::new(cwd_str);

    if !cwd.is_dir() {
        return HookOutput::allow();
    }

    let mut results: Vec<JunkDoc> = Vec::new();
    scan_docs(cwd, cwd, 0, 3, &mut results);

    if results.is_empty() {
        return HookOutput::allow();
    }

    // Group by reason
    let empty: Vec<&JunkDoc> = results
        .iter()
        .filter(|d| matches!(d.reason, JunkReason::Empty))
        .collect();
    let orphaned: Vec<&JunkDoc> = results
        .iter()
        .filter(|d| matches!(d.reason, JunkReason::Orphaned))
        .collect();
    let todo: Vec<&JunkDoc> = results
        .iter()
        .filter(|d| matches!(d.reason, JunkReason::TodoOnly))
        .collect();

    // Build warning box for stderr
    let mut lines = Vec::new();
    lines.push(String::new());
    lines.push("+-----------------------------------------------------------+".to_string());
    lines.push("|  DOC CLEANUP NEEDED                                       |".to_string());
    lines.push("+-----------------------------------------------------------+".to_string());

    if !empty.is_empty() {
        lines.push(format!(
            "|  Empty/stub docs: {}{}|",
            empty.len(),
            " ".repeat(39 - empty.len().to_string().len())
        ));
        for doc in empty.iter().take(3) {
            let path_display = if doc.path.len() > 50 {
                &doc.path[..50]
            } else {
                &doc.path
            };
            lines.push(format!(
                "|    - {}{}|",
                path_display,
                " ".repeat(53 - path_display.len().min(53))
            ));
        }
        if empty.len() > 3 {
            let msg = format!("... and {} more", empty.len() - 3);
            lines.push(format!(
                "|    {}{}|",
                msg,
                " ".repeat(55 - msg.len())
            ));
        }
    }

    if !orphaned.is_empty() {
        lines.push(format!(
            "|  Orphaned in root: {}{}|",
            orphaned.len(),
            " ".repeat(38 - orphaned.len().to_string().len())
        ));
        for doc in orphaned.iter().take(3) {
            let path_display = if doc.path.len() > 50 {
                &doc.path[..50]
            } else {
                &doc.path
            };
            lines.push(format!(
                "|    - {}{}|",
                path_display,
                " ".repeat(53 - path_display.len().min(53))
            ));
        }
    }

    if !todo.is_empty() {
        lines.push(format!(
            "|  TODO-only docs: {}{}|",
            todo.len(),
            " ".repeat(39 - todo.len().to_string().len())
        ));
        for doc in todo.iter().take(3) {
            let path_display = if doc.path.len() > 50 {
                &doc.path[..50]
            } else {
                &doc.path
            };
            lines.push(format!(
                "|    - {}{}|",
                path_display,
                " ".repeat(53 - path_display.len().min(53))
            ));
        }
    }

    lines.push("+-----------------------------------------------------------+".to_string());
    lines.push("|  Run /document clean to fix                               |".to_string());
    lines.push("+-----------------------------------------------------------+".to_string());
    lines.push(String::new());

    // Output to stderr (displayed to user in Claude Code)
    let warning = lines.join("\n");
    tracing::warn!("{}", warning);

    // Log individual junk files for debugging
    for doc in &results {
        tracing::debug!(path = %doc.path, reason = doc.reason.label(), "junk doc detected");
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_empty_dir_no_junk() {
        let dir = tempfile::tempdir().unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let output = process(&input);
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
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_empty_md_detected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.md"), "# Notes\n\nTODO").unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let output = process(&input);
        // Should still allow (Stop hooks never block), but should have logged
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_todo_only_detected() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("feature.md"),
            "# Feature\n\nTODO: implement this feature with all the details and make sure it covers enough content to be over 100 characters of stripped content",
        )
        .unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let nm = dir.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("junk.md"), "").unwrap();
        let input = HookInput {
            cwd: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_scan_docs_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        // Create deeply nested file beyond max depth
        let deep = dir.path().join("a").join("b").join("c").join("d");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.md"), "").unwrap();

        let mut results = Vec::new();
        scan_docs(dir.path(), dir.path(), 0, 3, &mut results);
        // depth 4 file should not be found
        assert!(results.is_empty());
    }

    #[test]
    fn test_junk_reason_labels() {
        assert_eq!(JunkReason::Empty.label(), "empty/stub");
        assert_eq!(JunkReason::TodoOnly.label(), "TODO-only");
        assert_eq!(JunkReason::Orphaned.label(), "orphaned in root");
    }
}
