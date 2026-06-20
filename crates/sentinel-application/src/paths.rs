//! Free-function path resolution for sites without a `FileSystemPort` in scope.
//!
//! Prefer [`sentinel_domain::ports::FileSystemPort::claude_dir`] when a
//! `&dyn FileSystemPort` is available — it lets tests mock the home dir.
//! Use these free functions only at top-level entry points (CLI commands,
//! init helpers) where threading a port would be excessive.
//!
//! Paths honor `SENTINEL_CLAUDE_DIR` for fully-isolated Claude-dir profiles and
//! `SENTINEL_HOME` for workspace-wide home-root isolation. Misconfigured
//! overrides or missing home directories fail closed; Sentinel must never write
//! state into the current working directory as a fallback.

use std::env::VarError;
use std::path::PathBuf;

/// Resolve the home root used by application-level path helpers.
///
/// `SENTINEL_HOME` wins when set. An explicit empty value is a configuration
/// error; otherwise Sentinel uses the OS home directory. This mirrors the
/// infrastructure path authority while keeping the application crate independent
/// from infrastructure.
pub fn home_root() -> Result<PathBuf, String> {
    match std::env::var("SENTINEL_HOME") {
        Ok(root) if !root.is_empty() => Ok(PathBuf::from(root)),
        Ok(_) => Err("SENTINEL_HOME is set but empty".to_string()),
        Err(VarError::NotPresent) => dirs::home_dir().ok_or_else(|| {
            "Cannot determine home directory. HOME/USERPROFILE must be set.".to_string()
        }),
        Err(VarError::NotUnicode(_)) => Err("SENTINEL_HOME is not valid Unicode".to_string()),
    }
}

/// Fatal form of [`home_root`].
pub fn home_root_or_fatal() -> PathBuf {
    home_root().unwrap_or_else(|e| panic!("[sentinel] FATAL: {e}"))
}

/// Resolve the Claude Code config/state directory.
///
/// Resolution order:
/// 1. `SENTINEL_CLAUDE_DIR` env var
/// 2. `SENTINEL_HOME/.claude`
/// 3. `$HOME/.claude` via [`dirs::home_dir`]
///
/// Fatal if `SENTINEL_CLAUDE_DIR` is set to an empty value or no home directory
/// is discoverable. That keeps path authority out of attacker-controlled CWDs.
pub fn claude_dir() -> PathBuf {
    try_claude_dir().unwrap_or_else(|e| panic!("[sentinel] FATAL: {e}"))
}

/// Fallible form of [`claude_dir`] for callers/tests that want to surface the
/// configuration error instead of panicking.
pub fn try_claude_dir() -> Result<PathBuf, String> {
    resolve_claude_dir(
        std::env::var("SENTINEL_CLAUDE_DIR"),
        std::env::var("SENTINEL_HOME"),
        dirs::home_dir(),
    )
}

fn resolve_claude_dir(
    env_override: Result<String, VarError>,
    home_override: Result<String, VarError>,
    home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    match env_override {
        Ok(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        Ok(_) => Err("SENTINEL_CLAUDE_DIR is set but empty".to_string()),
        Err(VarError::NotPresent) => match home_override {
            Ok(root) if !root.is_empty() => Ok(PathBuf::from(root).join(".claude")),
            Ok(_) => Err("SENTINEL_HOME is set but empty".to_string()),
            Err(VarError::NotPresent) => home.map(|home| home.join(".claude")).ok_or_else(|| {
                "Cannot determine home directory. HOME/USERPROFILE must be set.".to_string()
            }),
            Err(VarError::NotUnicode(_)) => Err("SENTINEL_HOME is not valid Unicode".to_string()),
        },
        Err(VarError::NotUnicode(_)) => Err("SENTINEL_CLAUDE_DIR is not valid Unicode".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize env-mutation tests within this module to avoid races with
    // each other. Cross-module races still possible — tests are correct
    // enough for the no-races single-threaded default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_envs<F: FnOnce()>(updates: &[(&'static str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous: Vec<(&'static str, Option<String>)> = updates
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect();
        for (key, value) in updates {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        for (key, value) in previous {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn env_override_wins() {
        with_envs(
            &[("SENTINEL_CLAUDE_DIR", Some("/tmp/sentinel-test-claude"))],
            || {
                assert_eq!(claude_dir(), PathBuf::from("/tmp/sentinel-test-claude"));
            },
        );
    }

    #[test]
    fn empty_env_falls_back_to_home() {
        with_envs(&[("SENTINEL_CLAUDE_DIR", Some(""))], || {
            let err = try_claude_dir().expect_err("empty override must fail closed");
            assert!(err.contains("SENTINEL_CLAUDE_DIR is set but empty"));
        });
    }

    #[test]
    fn unset_env_uses_home() {
        with_envs(
            &[("SENTINEL_CLAUDE_DIR", None), ("SENTINEL_HOME", None)],
            || {
                let resolved = claude_dir();
                assert!(resolved.ends_with(".claude"), "got: {resolved:?}");
            },
        );
    }

    #[test]
    fn sentinel_home_overrides_os_home_when_claude_dir_unset() {
        with_envs(
            &[
                ("SENTINEL_CLAUDE_DIR", None),
                ("SENTINEL_HOME", Some("/tmp/sentinel-home-root")),
            ],
            || {
                assert_eq!(
                    try_claude_dir().unwrap(),
                    PathBuf::from("/tmp/sentinel-home-root").join(".claude")
                );
            },
        );
    }

    #[test]
    fn empty_sentinel_home_fails_closed() {
        with_envs(
            &[("SENTINEL_CLAUDE_DIR", None), ("SENTINEL_HOME", Some(""))],
            || {
                let err = home_root().expect_err("empty SENTINEL_HOME must fail closed");
                assert!(err.contains("SENTINEL_HOME is set but empty"));
                let err = try_claude_dir().expect_err("empty SENTINEL_HOME must fail closed");
                assert!(err.contains("SENTINEL_HOME is set but empty"));
            },
        );
    }

    #[test]
    fn home_root_honors_sentinel_home() {
        with_envs(
            &[
                ("SENTINEL_CLAUDE_DIR", None),
                ("SENTINEL_HOME", Some("/tmp/sentinel-application-home")),
            ],
            || {
                assert_eq!(
                    home_root().unwrap(),
                    PathBuf::from("/tmp/sentinel-application-home")
                );
            },
        );
    }

    #[test]
    fn no_home_does_not_fall_back_to_cwd() {
        let err = resolve_claude_dir(Err(VarError::NotPresent), Err(VarError::NotPresent), None)
            .expect_err("missing home must fail closed");
        assert!(err.contains("Cannot determine home directory"));
    }

    #[test]
    fn resolver_uses_home_when_env_unset() {
        let home = PathBuf::from("/tmp/sentinel-home");
        assert_eq!(
            resolve_claude_dir(
                Err(VarError::NotPresent),
                Err(VarError::NotPresent),
                Some(home.clone())
            )
            .unwrap(),
            home.join(".claude")
        );
    }
}
