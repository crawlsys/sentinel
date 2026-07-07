//! Home-directory resolution for sentinel's on-disk state.
//!
//! Single source of truth for "where is the user's home". All engine paths
//! (`~/.claude/sentinel/{config,state}`, the FS adapter's `home_dir()`, etc.)
//! resolve through here so the entire engine can be redirected to an isolated
//! root with one env var.
//!
//! ## `SENTINEL_HOME`
//!
//! When `SENTINEL_HOME` is set, it overrides the OS home directory. An explicit
//! empty value is a configuration error, not a signal to fall back to OS home.
//! This exists for **test isolation**: `dirs::home_dir()` on Windows queries the
//! OS user-profile API and **ignores** the `HOME`/`USERPROFILE` env vars, so a
//! black-box test cannot isolate `~/.claude` by setting `HOME` alone. Routing
//! every home lookup through `SENTINEL_HOME` lets the E2E harness point the whole
//! engine at a tempdir on every platform. In production the var is unset and
//! behavior is identical to `dirs::home_dir()`.
//!
//! Note this is distinct from `SENTINEL_CLAUDE_DIR` (honored by
//! `FileSystemPort::claude_dir`), which overrides the `.claude` dir specifically;
//! `SENTINEL_HOME` overrides the home root used by all sentinel-owned
//! `~/.claude/sentinel` paths.

use std::path::PathBuf;

/// The resolved home root: `SENTINEL_HOME` if set, else the OS home.
///
/// Returns `None` when neither is available, or when `SENTINEL_HOME` is
/// explicitly empty. Use [`home_root_or_fatal`] when callers need the exact
/// configuration error.
#[must_use]
pub fn home_root() -> Option<PathBuf> {
    try_home_root().ok()
}

/// Fallible authoritative home-root resolver.
///
/// Reads the env + OS home dir here (the IO boundary) and delegates the
/// fail-closed decision to the shared, pure
/// [`sentinel_domain::paths::resolve_home_root`] — identical policy to
/// `sentinel-application::paths::home_root`, guaranteed by construction.
pub fn try_home_root() -> Result<PathBuf, String> {
    sentinel_domain::paths::resolve_home_root(std::env::var("SENTINEL_HOME"), dirs::home_dir())
}

/// Like [`home_root`] but panics with the standard FATAL message when no home is
/// resolvable — matches the existing fail-closed behavior of `config_dir` /
/// `state_dir` (Attack #84/#85: never write to CWD).
#[must_use]
pub fn home_root_or_fatal() -> PathBuf {
    try_home_root().unwrap_or_else(|e| panic!("[sentinel] FATAL: {e}"))
}

/// Root directory for sentinel-owned files under the user's Claude home.
#[must_use]
pub fn sentinel_root() -> PathBuf {
    home_root_or_fatal().join(".claude").join("sentinel")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn sentinel_home_overrides_when_set() {
        let _guard = env_lock();
        let tmp = std::env::temp_dir().join("sentinel-home-test-root");
        let _env = EnvGuard::set("SENTINEL_HOME", &tmp);
        assert_eq!(home_root(), Some(tmp.clone()));
        assert_eq!(home_root_or_fatal(), tmp);
    }

    #[test]
    fn sentinel_root_uses_authoritative_home_root() {
        let _guard = env_lock();
        let tmp = std::env::temp_dir().join("sentinel-root-test-root");
        let _env = EnvGuard::set("SENTINEL_HOME", &tmp);
        assert_eq!(sentinel_root(), tmp.join(".claude").join("sentinel"));
    }

    #[test]
    fn empty_sentinel_home_fails_closed() {
        let _guard = env_lock();
        let _env = EnvGuard::set("SENTINEL_HOME", "");
        assert_eq!(home_root(), None);
        assert_eq!(
            try_home_root().unwrap_err(),
            "SENTINEL_HOME is set but empty"
        );
    }
}
