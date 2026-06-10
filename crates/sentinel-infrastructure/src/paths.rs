//! Home-directory resolution for sentinel's on-disk state.
//!
//! Single source of truth for "where is the user's home". All engine paths
//! (`~/.claude/sentinel/{config,state}`, the FS adapter's `home_dir()`, etc.)
//! resolve through here so the entire engine can be redirected to an isolated
//! root with one env var.
//!
//! ## `SENTINEL_HOME`
//!
//! When `SENTINEL_HOME` is set (non-empty), it overrides the OS home directory.
//! This exists for **test isolation**: `dirs::home_dir()` on Windows queries the
//! OS user-profile API and **ignores** the `HOME`/`USERPROFILE` env vars, so a
//! black-box test cannot isolate `~/.claude` by setting `HOME` alone. Routing
//! every home lookup through `SENTINEL_HOME` lets the E2E harness point the whole
//! engine at a tempdir on every platform. In production the var is unset and
//! behavior is identical to `dirs::home_dir()`.
//!
//! Note this is distinct from `SENTINEL_CLAUDE_DIR` (honored by
//! `FileSystemPort::claude_dir`), which overrides the `.claude` dir specifically;
//! `SENTINEL_HOME` overrides the home root that everything (including the legacy
//! `home_dir().join(".claude")` call sites) derives from.

use std::path::PathBuf;

/// The resolved home root: `SENTINEL_HOME` if set (non-empty), else the OS home.
/// Returns `None` only when neither is available (same as `dirs::home_dir`).
#[must_use]
pub fn home_root() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("SENTINEL_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir()
}

/// Like [`home_root`] but panics with the standard FATAL message when no home is
/// resolvable — matches the existing fail-closed behavior of `config_dir` /
/// `state_dir` (Attack #84/#85: never fall back to CWD).
#[must_use]
pub fn home_root_or_fatal() -> PathBuf {
    home_root()
        .expect("[sentinel] FATAL: Cannot determine home directory. HOME/USERPROFILE must be set.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_home_overrides_when_set() {
        // Serialize via a single test (env is process-global) — set, assert, clear.
        let tmp = std::env::temp_dir().join("sentinel-home-test-root");
        // SAFETY: single-threaded test; restored before returning.
        std::env::set_var("SENTINEL_HOME", &tmp);
        assert_eq!(home_root(), Some(tmp.clone()));
        assert_eq!(home_root_or_fatal(), tmp);
        std::env::remove_var("SENTINEL_HOME");
        // After clearing, falls back to the OS home (non-None on a real box).
        assert_eq!(home_root(), dirs::home_dir());
    }

    #[test]
    fn empty_sentinel_home_is_ignored() {
        std::env::set_var("SENTINEL_HOME", "");
        assert_eq!(home_root(), dirs::home_dir());
        std::env::remove_var("SENTINEL_HOME");
    }
}
