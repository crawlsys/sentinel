//! Real filesystem adapter — implements `FileSystemPort`.
//!
//! Thin delegation to `std::fs` + dirs. Exists so hooks can be tested
//! with a mock filesystem that doesn't touch real disk.

use anyhow::{Context, Result};
use sentinel_domain::ports::FileSystemPort;
use std::path::{Path, PathBuf};

/// Infrastructure adapter implementing `FileSystemPort` via real `std::fs`.
pub struct RealFileSystem;

impl FileSystemPort for RealFileSystem {
    fn home_dir(&self) -> Option<PathBuf> {
        dirs::home_dir()
    }

    fn read_to_string(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
    }

    fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        std::fs::write(path, content).with_context(|| format!("write {}", path.display()))
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path).with_context(|| format!("create_dir_all {}", path.display()))
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let entries = std::fs::read_dir(path)
            .with_context(|| format!("read_dir {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        Ok(entries)
    }

    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::remove_dir_all(path).with_context(|| format!("remove_dir_all {}", path.display()))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        std::fs::metadata(path).with_context(|| format!("metadata {}", path.display()))
    }

    fn append(&self, path: &Path, content: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        // Best-effort rotation: if this is an observability metrics JSONL
        // and the file has crossed the size cap, archive it before the
        // next append. Only sentinel/metrics/*.jsonl paths are rotated;
        // other appends (state markers, manifests, etc.) are untouched.
        rotate_metrics_log_if_oversized(path);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("append open {}", path.display()))?;
        file.write_all(content)
            .with_context(|| format!("append write {}", path.display()))
    }

    fn copy(&self, src: &Path, dst: &Path) -> Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        std::fs::copy(src, dst)
            .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            // Treat "not found" as success — callers use this for best-effort
            // cleanup of state markers that may not exist yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("remove_file {}", path.display()))),
        }
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("remove_dir {}", path.display()))),
        }
    }
}

/// Cap on size of a sentinel observability metrics JSONL file before we
/// rotate it. 10 MB is enough for normal observability load (the busiest
/// real file, mcp-supervisor.jsonl, hit ~13 MB only when an orphaned
/// process spammed it for weeks; healthy steady-state is well under this)
/// while small enough to keep tooling like `tail -F`, `grep`, and
/// readline-style diagnostics responsive.
const METRICS_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Detect whether a path is a sentinel observability metrics JSONL.
/// Pure function — no IO. Match is intentionally restrictive so we
/// don't accidentally rotate state markers, manifests, or any other
/// `.jsonl` file outside the metrics directory.
fn is_metrics_jsonl(path: &Path) -> bool {
    let s = path.to_string_lossy();
    // Match both `/sentinel/metrics/` (Unix) and `\sentinel\metrics\` (Windows)
    let in_metrics = s.contains("sentinel/metrics/")
        || s.contains("sentinel\\metrics\\");
    let is_jsonl = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "jsonl")
        .unwrap_or(false);
    in_metrics && is_jsonl
}

/// Best-effort: if `path` is a metrics JSONL larger than
/// `METRICS_LOG_MAX_BYTES`, rename it to `<file>.archive.<ts_ms>` so the
/// next append starts a fresh file. Errors are swallowed — observability
/// plumbing must not break the caller's critical path.
///
/// Public so the unit tests can exercise the path-classifier + size
/// threshold logic in isolation. Not part of `FileSystemPort`; consumed
/// only by `RealFileSystem::append`.
pub fn rotate_metrics_log_if_oversized(path: &Path) {
    if !is_metrics_jsonl(path) {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() <= METRICS_LOG_MAX_BYTES {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let archive_name = format!(
        "{}.archive.{ts}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("rotated"),
    );
    let archive_path = path.with_file_name(archive_name);
    let _ = std::fs::rename(path, archive_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_home_dir_exists() {
        let fs = RealFileSystem;
        assert!(fs.home_dir().is_some());
    }

    #[test]
    fn test_exists_and_is_dir() {
        let fs = RealFileSystem;
        let tmp = std::env::temp_dir();
        assert!(fs.exists(&tmp));
        assert!(fs.is_dir(&tmp));
        assert!(!fs.exists(Path::new("/nonexistent/path/xyz")));
    }

    #[test]
    fn test_write_and_read() {
        let fs = RealFileSystem;
        let tmp = std::env::temp_dir().join("sentinel-fs-port-test.txt");
        fs.write(&tmp, b"hello world").unwrap();
        let content = fs.read_to_string(&tmp).unwrap();
        assert_eq!(content, "hello world");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_read_dir() {
        let fs = RealFileSystem;
        let tmp = std::env::temp_dir();
        let entries = fs.read_dir(&tmp).unwrap();
        assert!(!entries.is_empty());
    }

    // ── metrics-log rotation ────────────────────────────────────────

    /// Pure-function classifier: only sentinel/metrics/*.jsonl matches.
    /// State markers, manifest files, and any non-jsonl file under
    /// metrics MUST NOT trigger rotation.
    #[test]
    fn is_metrics_jsonl_classifier() {
        assert!(is_metrics_jsonl(Path::new(
            "/c/Users/garys/.claude/sentinel/metrics/sessions.jsonl"
        )));
        assert!(is_metrics_jsonl(Path::new(
            "C:\\Users\\garys\\.claude\\sentinel\\metrics\\errors.jsonl"
        )));
        // Wrong extension
        assert!(!is_metrics_jsonl(Path::new(
            "/sentinel/metrics/state.json"
        )));
        // Wrong directory
        assert!(!is_metrics_jsonl(Path::new(
            "/.claude/sentinel/state/markers.jsonl"
        )));
        // Sibling-of-metrics (not under it)
        assert!(!is_metrics_jsonl(Path::new(
            "/sentinel/metrics-archive/old.jsonl"
        )));
    }

    #[test]
    fn rotate_skips_non_metrics_path() {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-rotate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // path NOT under sentinel/metrics — rotation must not touch it
        // even if it's huge.
        let path = dir.join("not-metrics.jsonl");
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        rotate_metrics_log_if_oversized(&path);
        assert!(path.exists(), "non-metrics path must not be rotated");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotate_metrics_under_cap_no_op() {
        // Construct a path that LOOKS like sentinel/metrics so the
        // classifier matches. Use a unique parent dir under tmp to avoid
        // clobbering any real metrics file.
        let dir = std::env::temp_dir()
            .join(format!("rt-under-{}", std::process::id()))
            .join("sentinel")
            .join("metrics");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        std::fs::write(&path, b"small").unwrap();
        rotate_metrics_log_if_oversized(&path);
        assert!(path.exists());
        std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn rotate_metrics_over_cap_archives() {
        let dir = std::env::temp_dir()
            .join(format!("rt-over-{}", std::process::id()))
            .join("sentinel")
            .join("metrics");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        // Write a file just over the cap. Use a small cap workaround:
        // we can't easily change METRICS_LOG_MAX_BYTES at test time, so
        // write 11 MB which is just over the 10 MB threshold.
        std::fs::write(&path, vec![b'x'; (METRICS_LOG_MAX_BYTES + 1024) as usize]).unwrap();
        rotate_metrics_log_if_oversized(&path);
        // Original gone, exactly one archive sibling.
        assert!(!path.exists(), "oversized metrics file should be renamed");
        let archives: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("test.jsonl.archive.")
            })
            .collect();
        assert_eq!(archives.len(), 1);
        std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn rotate_metrics_missing_file_no_op() {
        let path = std::env::temp_dir()
            .join("sentinel")
            .join("metrics")
            .join(format!("nonexistent-{}.jsonl", std::process::id()));
        // Should not panic and should not error; just returns silently.
        rotate_metrics_log_if_oversized(&path);
        assert!(!path.exists());
    }
}
