//! Repository-kind classification.
//!
//! Two related rules used by `pre_commit_verification`:
//!
//! 1. **Is this a content-only repo?** — has no build-config marker files,
//!    so there's no test toolchain to invoke. The list of markers
//!    [`BUILD_CONFIG_MARKERS`] is the domain rule; the IO check ("does
//!    file X exist?") stays in the hook (so the hook can keep using
//!    `FileSystemPort` for testability).
//! 2. **Does this changed-file path look docs-only?** —
//!    [`is_docs_only_path`] checks the extension against
//!    [`DOCS_ONLY_EXTENSIONS`]. Pure rule, no IO.

/// Build-config marker filenames. Their presence in a repo root indicates
/// the repo has a test/build toolchain that pre-commit verification can
/// invoke; their absence means the repo is content-only.
///
/// **Add a marker if** the language ecosystem has a canonical config file
/// (`Cargo.toml`, `package.json`, etc.). Don't add lockfiles or per-tool
/// configs that may exist alongside a richer marker — over-detection
/// produces false positives ("this content repo also has `.eslintrc`").
pub const BUILD_CONFIG_MARKERS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "go.mod",
    "Makefile",
    "makefile",
    "Dockerfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Gemfile",
    "mix.exs",
    "CMakeLists.txt",
];

/// File extensions / filenames that count as "docs only". A commit/push
/// touching ONLY paths matching this list bypasses pre-commit verification
/// — there's no executable code change to test.
///
/// Lower-case, leading dot for extensions. Bare-name entries
/// (`LICENSE`, `CHANGELOG`, `SECURITY`) are matched as a substring of the
/// path so that `LICENSE-MIT` and `docs/CHANGELOG.md` (which would also
/// match via `.md`) both qualify.
pub const DOCS_ONLY_EXTENSIONS: &[&str] = &[
    ".md",
    ".mdx",
    ".txt",
    ".json",
    ".yaml",
    ".yml",
    ".toml",
    ".ini",
    ".cfg",
    ".conf",
    ".env",
    ".env.example",
    ".editorconfig",
    ".gitignore",
    ".gitattributes",
    ".prettierrc",
    ".eslintrc",
    ".dockerignore",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
    ".ruby-version",
    ".python-version",
    ".csv",
    ".tsv",
    ".xml",
    ".svg",
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".ico",
    ".woff",
    ".woff2",
    ".ttf",
    ".eot",
    ".otf",
    "LICENSE",
    "CHANGELOG",
    "SECURITY",
];

/// Return `true` if `path` matches any entry in [`DOCS_ONLY_EXTENSIONS`]
/// using case-insensitive suffix match (the same rule the hook used
/// inline).
#[must_use]
pub fn is_docs_only_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    DOCS_ONLY_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(&ext.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_typical_docs_files() {
        assert!(is_docs_only_path("README.md"));
        assert!(is_docs_only_path("docs/architecture.md"));
        assert!(is_docs_only_path("CHANGELOG.md"));
        assert!(is_docs_only_path("a/b/c.txt"));
        assert!(is_docs_only_path("config.toml"));
        assert!(is_docs_only_path("Cargo.toml"));
        assert!(is_docs_only_path("project.yaml"));
    }

    #[test]
    fn matches_image_assets() {
        assert!(is_docs_only_path("assets/logo.png"));
        assert!(is_docs_only_path("hero.jpg"));
        assert!(is_docs_only_path("icon.svg"));
        assert!(is_docs_only_path("font.woff2"));
    }

    #[test]
    fn matches_bare_filenames_as_suffix() {
        // The list contains bare entries (no leading dot) like LICENSE.
        // Paths ending with the bare name (case-insensitive) match.
        assert!(is_docs_only_path("LICENSE"));
        assert!(is_docs_only_path("docs/CHANGELOG"));
        // Also matches as a path-tail.
        assert!(is_docs_only_path("subdir/SECURITY"));
    }

    #[test]
    fn case_insensitive_match() {
        assert!(is_docs_only_path("README.MD"));
        assert!(is_docs_only_path("ICON.PNG"));
    }

    #[test]
    fn rejects_code_extensions() {
        assert!(!is_docs_only_path("src/main.rs"));
        assert!(!is_docs_only_path("app/Page.tsx"));
        assert!(!is_docs_only_path("lib/utils.py"));
        assert!(!is_docs_only_path("server.go"));
    }

    #[test]
    fn rejects_paths_without_recognized_extension() {
        assert!(!is_docs_only_path("script"));
        assert!(!is_docs_only_path("Dockerfile"));
        assert!(!is_docs_only_path("Gemfile"));
    }

    #[test]
    fn extension_must_be_at_end() {
        // `*.md` doesn't appear mid-path.
        assert!(!is_docs_only_path("docs.md/foo.bin"));
    }

    #[test]
    fn build_config_markers_list_is_non_empty() {
        // Sanity — the security boundary depends on this list being
        // populated. If a refactor accidentally empties it, every
        // repo would look content-only and verification would skip.
        assert!(!BUILD_CONFIG_MARKERS.is_empty());
        // The most common ones must be present.
        assert!(BUILD_CONFIG_MARKERS.contains(&"package.json"));
        assert!(BUILD_CONFIG_MARKERS.contains(&"Cargo.toml"));
        assert!(BUILD_CONFIG_MARKERS.contains(&"pyproject.toml"));
    }
}
