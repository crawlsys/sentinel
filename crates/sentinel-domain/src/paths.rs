//! Pure path-resolution policy shared across the crates.
//!
//! The *decision* logic for where sentinel's home root lives is a single
//! fail-closed policy that both `sentinel-application` and
//! `sentinel-infrastructure` must apply identically — a divergence would let
//! one crate resolve state to a different directory than the other. Rather
//! than hand-duplicate the `match` in each (the historical shape), the policy
//! lives here as a pure function: the caller performs the IO (reading
//! `SENTINEL_HOME`, calling `dirs::home_dir()`) and passes the results in, so
//! this module stays free of IO and honours domain purity.

use std::env::VarError;
use std::path::PathBuf;

/// Resolve the sentinel home root from already-read inputs.
///
/// Policy (fail-closed):
/// 1. `SENTINEL_HOME` set to a non-empty value wins — workspace-wide isolation.
/// 2. `SENTINEL_HOME` set but **empty** is a configuration error, not a
///    fall-through — returns `Err` so the caller fails loudly instead of
///    silently resolving to the real home.
/// 3. `SENTINEL_HOME` absent → the platform home directory, if resolvable.
/// 4. `SENTINEL_HOME` present but non-Unicode, or no home directory → `Err`.
///
/// `sentinel_home` is the result of reading the `SENTINEL_HOME` env var;
/// `home_dir` is the result of `dirs::home_dir()` (or an equivalent). No IO is
/// performed here.
///
/// # Errors
/// Returns a human-readable message when the home root cannot be determined:
/// an empty `SENTINEL_HOME`, a non-Unicode `SENTINEL_HOME`, or an absent home
/// directory.
pub fn resolve_home_root(
    sentinel_home: Result<String, VarError>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf, String> {
    match sentinel_home {
        Ok(root) if !root.is_empty() => Ok(PathBuf::from(root)),
        Ok(_) => Err("SENTINEL_HOME is set but empty".to_string()),
        Err(VarError::NotPresent) => home_dir.ok_or_else(|| {
            "Cannot determine home directory. HOME/USERPROFILE must be set.".to_string()
        }),
        Err(VarError::NotUnicode(_)) => Err("SENTINEL_HOME is not valid Unicode".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_home_wins_when_set() {
        let got = resolve_home_root(Ok("/tmp/sh".to_string()), Some(PathBuf::from("/home/x")));
        assert_eq!(got.unwrap(), PathBuf::from("/tmp/sh"));
    }

    #[test]
    fn empty_sentinel_home_fails_closed_not_fallthrough() {
        // Critical: an empty override must NOT silently resolve to the real home.
        let err = resolve_home_root(Ok(String::new()), Some(PathBuf::from("/home/x")))
            .expect_err("empty SENTINEL_HOME must be an error");
        assert!(err.contains("empty"));
    }

    #[test]
    fn absent_sentinel_home_uses_home_dir() {
        let got = resolve_home_root(Err(VarError::NotPresent), Some(PathBuf::from("/home/x")));
        assert_eq!(got.unwrap(), PathBuf::from("/home/x"));
    }

    #[test]
    fn absent_sentinel_home_and_no_home_dir_fails() {
        let err = resolve_home_root(Err(VarError::NotPresent), None)
            .expect_err("no home directory must fail closed");
        assert!(err.contains("Cannot determine home directory"));
    }

    #[test]
    fn non_unicode_sentinel_home_fails() {
        // Construct a NotUnicode VarError without platform-specific OsString tricks
        // by round-tripping through an intentionally-invalid read is not portable;
        // instead assert the arm via a direct match on a synthesized value.
        #[cfg(windows)]
        let bad = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0xD800]) // lone surrogate = invalid unicode
        };
        #[cfg(unix)]
        let bad = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![0xFF, 0xFE]) // invalid utf-8
        };
        let err = resolve_home_root(Err(VarError::NotUnicode(bad)), Some(PathBuf::from("/h")))
            .expect_err("non-unicode SENTINEL_HOME must fail");
        assert!(err.contains("not valid Unicode"));
    }
}
