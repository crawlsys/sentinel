//! File-kind classification.
//!
//! Domain rules for classifying files by their extension. Currently used
//! by `hygiene_reminders` to decide whether changed files indicate a code
//! change (and therefore a possibly-stale CHANGELOG).

/// Extensions that count as "source code" for the purposes of CHANGELOG-
/// staleness reminders. Lower-case, no leading dot.
///
/// Intentionally narrow: the goal is to flag obvious source-file edits
/// without nagging on every JSON/YAML/TOML config tweak.
pub const CODE_EXTENSIONS: &[&str] =
    &["rs", "ts", "tsx", "js", "jsx", "py", "go"];

/// Return `true` if `path` ends with any of [`CODE_EXTENSIONS`]. Match is
/// case-insensitive on the extension, so `Cargo.toml.RS` matches `rs`.
///
/// `path` is taken as a `&str` because the call sites have raw paths from
/// git status output (which preserves case). This stays a free function —
/// it doesn't justify a `Path`-typed wrapper for one-line work.
#[must_use]
pub fn is_code_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    CODE_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_each_extension() {
        for ext in CODE_EXTENSIONS {
            let path = format!("src/foo.{ext}");
            assert!(is_code_file(&path), "expected .{ext} to match");
        }
    }

    #[test]
    fn case_insensitive_extension() {
        assert!(is_code_file("foo.RS"));
        assert!(is_code_file("Foo.Tsx"));
        assert!(is_code_file("BAR.PY"));
    }

    #[test]
    fn rejects_non_code_extensions() {
        assert!(!is_code_file("readme.md"));
        assert!(!is_code_file("Cargo.toml"));
        assert!(!is_code_file("config.yaml"));
        assert!(!is_code_file("data.json"));
        assert!(!is_code_file("LICENSE"));
    }

    #[test]
    fn requires_dot_separator() {
        // Bare suffix without the dot doesn't match — `foors` is not
        // a Rust file.
        assert!(!is_code_file("foors"));
        assert!(!is_code_file("ts"));
    }

    #[test]
    fn handles_paths_with_dirs() {
        assert!(is_code_file("src/lib/foo.rs"));
        assert!(is_code_file("./components/Button.tsx"));
        assert!(is_code_file("a\\b\\c.py"));
    }
}
