//! Real filesystem adapter — implements `FileSystemPort`.
//!
//! Thin delegation to `std::fs` + dirs. Exists so hooks can be tested
//! with a mock filesystem that doesn't touch real disk.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sentinel_domain::ports::FileSystemPort;

/// Infrastructure adapter implementing `FileSystemPort` via real `std::fs`.
pub struct RealFileSystem;

impl FileSystemPort for RealFileSystem {
    fn home_dir(&self) -> Option<PathBuf> {
        // Routes through `paths::home_root()` so the whole engine (and all 100+
        // `home_dir().join(".claude")` hook call sites) can be redirected to an
        // isolated root via `SENTINEL_HOME` — required for cross-platform test
        // isolation, since `dirs::home_dir()` ignores HOME/USERPROFILE on Windows.
        crate::paths::home_root()
    }

    fn claude_dir(&self) -> PathBuf {
        if let Ok(dir) = std::env::var("SENTINEL_CLAUDE_DIR") {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
        self.home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".claude")
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
        // Best-effort trace_id stamping: if this is a metrics JSONL line
        // and the payload is a parseable JSON object that doesn't already
        // carry `trace_id`, inject it from the env var (or mint one) so
        // every event in `~/.claude/sentinel/metrics/*.jsonl` shares the
        // same correlation id as the handler-side launch event for the
        // current operation. Malformed/multi-doc lines write unchanged.
        let stamped: Vec<u8>;
        let payload: &[u8] = if is_metrics_jsonl(path) {
            stamped = stamp_trace_id_if_missing(content);
            &stamped
        } else {
            content
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("append open {}", path.display()))?;
        file.write_all(payload)
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
            Err(e) => Err(anyhow::Error::new(e).context(format!("remove_file {}", path.display()))),
        }
    }

    fn remove_dir(&self, path: &Path) -> Result<()> {
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context(format!("remove_dir {}", path.display()))),
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
    let in_metrics = s.contains("sentinel/metrics/") || s.contains("sentinel\\metrics\\");
    let is_jsonl = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "jsonl");
    in_metrics && is_jsonl
}

/// Env var that carries the current trace id between processes. Read
/// from the inherited env when the handler spawned us; minted fresh
/// when sentinel is the start of the chain (e.g. an interactive
/// `accounts` CLI invocation that didn't go through `c`).
///
/// CONTRACT (cross-crate, must stay in sync — single source of truth
/// is the env-var name, not the helper impl):
/// - Var name: literal `"CLAUDE_TRACE_ID"`. Same string is duplicated in
///   `accounts-application/src/trace.rs::TRACE_ID_ENV_VAR` (public). If
///   one ever changes, both must.
/// - Empty/whitespace value is treated as unset.
/// - Fallback is a fresh `UUIDv4` string in 8-4-4-4-12 hex with version
///   bits 4 and RFC 4122 variant bits set. accounts-application uses
///   `mint_token_lineage` (inline format); sentinel uses the `uuid`
///   crate's `Uuid::new_v4().to_string()`. Same wire shape.
/// - Result is a String, no validation on read (callers don't care
///   whether the inherited value is a valid UUID — they care only
///   that two events with the same `trace_id` came from the same
///   user-initiated operation).
const TRACE_ID_ENV_VAR: &str = "CLAUDE_TRACE_ID";

/// Read `CLAUDE_TRACE_ID` from the env, or mint a fresh `UUIDv4` if absent.
/// Wrapped in a one-line helper so the append path can call it without
/// every caller knowing about the env-var contract.
fn current_trace_id() -> String {
    current_trace_id_from(|| std::env::var(TRACE_ID_ENV_VAR).ok())
}

/// Pure-function variant for testing. The caller supplies an env-var
/// reader closure so tests can inject Some/None without mutating
/// process-global env (which violates `unsafe_code = forbid` in this
/// workspace).
fn current_trace_id_from<F: FnOnce() -> Option<String>>(read_env: F) -> String {
    match read_env() {
        Some(s) if !s.trim().is_empty() => s,
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

/// Stamp `trace_id` onto a JSONL line if it parses as a single JSON
/// object that doesn't already carry one. Returns the (possibly
/// modified) line as bytes, with the trailing newline preserved.
///
/// Defensive on every edge case — observability plumbing must not break
/// the caller's critical path:
/// - Empty buffer or just a newline → unchanged.
/// - Multi-line payload (rare; one writer batched several events) →
///   each line stamped independently.
/// - Non-JSON line, JSON array, JSON scalar → unchanged.
/// - Already has `trace_id` → unchanged (caller wins).
/// - Reserialization fails → original returned.
fn stamp_trace_id_if_missing(content: &[u8]) -> Vec<u8> {
    stamp_trace_id_if_missing_with(content, current_trace_id)
}

/// Pure-function variant for testing. The caller supplies the `trace_id`
/// generator so tests can pass deterministic ids without mutating
/// process-global env (`unsafe_code = forbid` in this workspace).
fn stamp_trace_id_if_missing_with<F: Fn() -> String>(content: &[u8], gen_id: F) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(content) else {
        return content.to_vec();
    };
    let mut out = String::with_capacity(text.len() + 64);
    let mut needs_stamp = false;
    for raw in text.split_inclusive('\n') {
        // split_inclusive keeps the trailing '\n'; strip it before parse,
        // then put it back exactly as it was.
        let (body, nl) = raw.strip_suffix('\n').map_or((raw, ""), |b| (b, "\n"));
        if body.is_empty() {
            out.push_str(raw);
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(serde_json::Value::Object(mut map)) => {
                if !map.contains_key("trace_id") {
                    map.insert(
                        "trace_id".to_string(),
                        serde_json::Value::String(gen_id()),
                    );
                    needs_stamp = true;
                }
                match serde_json::to_string(&serde_json::Value::Object(map)) {
                    Ok(s) => {
                        out.push_str(&s);
                        out.push_str(nl);
                    }
                    // Reserialization should be infallible for a parsed
                    // Value, but if it ever isn't, keep the original line.
                    Err(_) => out.push_str(raw),
                }
            }
            // Unparseable, array, or scalar — pass through unchanged.
            _ => out.push_str(raw),
        }
    }
    if needs_stamp {
        out.into_bytes()
    } else {
        content.to_vec()
    }
}

/// Best-effort metrics log rotation.
///
/// If `path` is a metrics JSONL larger than `METRICS_LOG_MAX_BYTES`, renames it
/// to `<file>.archive.<ts_ms>` so the next append starts a fresh file. Errors
/// are swallowed — observability plumbing must not break the caller's critical
/// path.
///
/// Public so the unit tests can exercise the path-classifier + size threshold
/// logic in isolation. Not part of `FileSystemPort`; consumed only by
/// `RealFileSystem::append`.
pub fn rotate_metrics_log_if_oversized(path: &Path) {
    if !is_metrics_jsonl(path) {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= METRICS_LOG_MAX_BYTES {
        return;
    }
    let ts = {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        // u128→u64: millis since epoch won't overflow u64 for ~584 million years
        #[allow(clippy::cast_possible_truncation)]
        let ts_u64 = millis as u64;
        ts_u64
    };
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
        assert!(!is_metrics_jsonl(Path::new("/sentinel/metrics/state.json")));
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
                .map_or(0, |d| d.as_nanos()),
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
            .filter_map(std::result::Result::ok)
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

    // ── trace_id stamping ───────────────────────────────────────────

    // All these tests use the pure-function variants
    // (`stamp_trace_id_if_missing_with` / `current_trace_id_from`) that
    // take an injected id-generator instead of touching real env vars.
    // This keeps the workspace `unsafe_code = forbid` lint clean.
    fn fixed_id(s: &'static str) -> impl Fn() -> String {
        move || s.to_string()
    }

    #[test]
    fn stamp_injects_trace_id_when_missing() {
        let stamped = stamp_trace_id_if_missing_with(
            b"{\"event\":\"foo\"}\n",
            fixed_id("test-stamp-fixed-id"),
        );
        let s = String::from_utf8(stamped).unwrap();
        assert!(s.contains("\"trace_id\":\"test-stamp-fixed-id\""), "got {s}");
        assert!(s.contains("\"event\":\"foo\""));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn stamp_preserves_existing_trace_id() {
        let stamped = stamp_trace_id_if_missing_with(
            b"{\"event\":\"foo\",\"trace_id\":\"caller-id\"}\n",
            fixed_id("env-id-should-not-win"),
        );
        let s = String::from_utf8(stamped).unwrap();
        assert!(s.contains("\"trace_id\":\"caller-id\""));
        assert!(!s.contains("env-id-should-not-win"));
    }

    #[test]
    fn stamp_passes_through_unparseable_lines() {
        let raw = b"not even json\n";
        let stamped = stamp_trace_id_if_missing_with(raw, fixed_id("trace-fixed"));
        assert_eq!(stamped, raw.to_vec());
    }

    #[test]
    fn stamp_passes_through_json_array() {
        let raw = b"[1,2,3]\n";
        let stamped = stamp_trace_id_if_missing_with(raw, fixed_id("trace-fixed"));
        assert_eq!(stamped, raw.to_vec());
    }

    #[test]
    fn stamp_handles_multiple_lines_independently() {
        let raw = b"{\"a\":1}\n{\"b\":2,\"trace_id\":\"already-set\"}\nbroken line\n";
        let stamped = stamp_trace_id_if_missing_with(raw, fixed_id("multi-line-id"));
        let s = String::from_utf8(stamped).unwrap();
        // First line gets stamped
        assert!(s.contains("\"a\":1") && s.contains("\"trace_id\":\"multi-line-id\""));
        // Second line keeps its own trace_id
        assert!(s.contains("\"b\":2") && s.contains("\"trace_id\":\"already-set\""));
        // Third line passes through verbatim
        assert!(s.contains("broken line"));
    }

    #[test]
    fn stamp_no_op_if_no_objects_need_stamping() {
        let raw = b"{\"trace_id\":\"x\"}\n";
        let stamped = stamp_trace_id_if_missing_with(raw, fixed_id("trace-fixed"));
        // Returns the original buffer when no edit happened — preserves
        // byte-for-byte equality (no reserialization-induced reformatting).
        assert_eq!(stamped, raw.to_vec());
    }

    #[test]
    fn current_trace_id_from_uses_supplied_value() {
        assert_eq!(
            current_trace_id_from(|| Some("env-supplied-id".into())),
            "env-supplied-id"
        );
    }

    #[test]
    fn current_trace_id_from_mints_uuid_when_env_unset() {
        let id = current_trace_id_from(|| None);
        // UUIDv4 shape: 8-4-4-4-12 hex
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
    }

    #[test]
    fn current_trace_id_from_treats_blank_as_missing() {
        let id = current_trace_id_from(|| Some("   ".into()));
        // Blank/whitespace value is treated as unset → fresh UUID
        assert_eq!(id.len(), 36);
    }

    /// Cross-crate contract regression test (task #18). The literal
    /// env-var name MUST match what `accounts-application` reads.
    /// If this string ever changes here, the matching constant in
    /// `accounts-application/src/trace.rs::TRACE_ID_ENV_VAR` must
    /// change in lock-step or events emitted by handler-side launches
    /// will stop being correlated with sentinel hook events.
    #[test]
    fn env_var_name_is_stable() {
        assert_eq!(TRACE_ID_ENV_VAR, "CLAUDE_TRACE_ID",
            "TRACE_ID_ENV_VAR is part of the cross-crate contract — \
             change requires matching update in accounts-application");
    }
}
