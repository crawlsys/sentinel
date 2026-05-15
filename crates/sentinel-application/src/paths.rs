//! Free-function path resolution for sites without a `FileSystemPort` in scope.
//!
//! Prefer [`sentinel_domain::ports::FileSystemPort::claude_dir`] when a
//! `&dyn FileSystemPort` is available — it lets tests mock the home dir.
//! Use these free functions only at top-level entry points (CLI commands,
//! init helpers) where threading a port would be excessive.
//!
//! Both paths honor `SENTINEL_CLAUDE_DIR` for fully-isolated sandbox profiles.

use std::path::PathBuf;

/// Resolve the Claude Code config/state directory.
///
/// Resolution order:
/// 1. `SENTINEL_CLAUDE_DIR` env var (if set and non-empty)
/// 2. `$HOME/.claude` via [`dirs::home_dir`]
/// 3. `./.claude` fallback when no home is discoverable
pub fn claude_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SENTINEL_CLAUDE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize env-mutation tests within this module to avoid races with
    // each other. Cross-module races still possible — tests are correct
    // enough for the no-races single-threaded default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(value: Option<&str>, f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("SENTINEL_CLAUDE_DIR").ok();
        match value {
            Some(v) => std::env::set_var("SENTINEL_CLAUDE_DIR", v),
            None => std::env::remove_var("SENTINEL_CLAUDE_DIR"),
        }
        f();
        match prev {
            Some(v) => std::env::set_var("SENTINEL_CLAUDE_DIR", v),
            None => std::env::remove_var("SENTINEL_CLAUDE_DIR"),
        }
    }

    #[test]
    fn env_override_wins() {
        with_env(Some("/tmp/sentinel-test-claude"), || {
            assert_eq!(claude_dir(), PathBuf::from("/tmp/sentinel-test-claude"));
        });
    }

    #[test]
    fn empty_env_falls_back_to_home() {
        with_env(Some(""), || {
            let resolved = claude_dir();
            assert!(resolved.ends_with(".claude"), "got: {resolved:?}");
        });
    }

    #[test]
    fn unset_env_uses_home() {
        with_env(None, || {
            let resolved = claude_dir();
            assert!(resolved.ends_with(".claude"), "got: {resolved:?}");
        });
    }
}
