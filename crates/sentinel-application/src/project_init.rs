//! Project initialization — audit and generate standard project files
//!
//! Detects project type from Cargo.toml/package.json, audits which standard
//! files exist, and generates missing ones from templates.
//!
//! Used by `sentinel init` CLI subcommand.

use std::fs;
use std::path::{Path, PathBuf};

use sentinel_domain::project::{
    AuditResult, InitResult, ProjectMetadata, ProjectType, RustFlavor, StandardFile,
};

// ---------------------------------------------------------------------------
// Metadata extraction
// ---------------------------------------------------------------------------

/// Extract project metadata from manifest files in the given directory.
pub fn extract_metadata(repo: &Path) -> ProjectMetadata {
    let mut meta = ProjectMetadata::default();

    let cargo_toml = repo.join("Cargo.toml");
    let package_json = repo.join("package.json");

    let has_cargo = cargo_toml.exists();
    let has_package = package_json.exists();

    meta.project_type = match (has_cargo, has_package) {
        (true, true) => ProjectType::Mixed,
        (true, false) => ProjectType::RustBinary,
        (false, true) => ProjectType::Node,
        (false, false) => ProjectType::Unknown,
    };

    // Parse Cargo.toml
    if has_cargo {
        if let Ok(content) = fs::read_to_string(&cargo_toml) {
            parse_cargo_toml(&content, &mut meta);
        }
    }

    // Parse package.json (basic — name/description/version)
    if has_package && meta.name.is_empty() {
        if let Ok(content) = fs::read_to_string(&package_json) {
            parse_package_json(&content, &mut meta);
        }
    }

    // Fallback: use directory name
    if meta.name.is_empty() {
        meta.name = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
    }

    // Infer repository URL from directory name
    if meta.repository.is_none() {
        let dir_name = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !dir_name.is_empty() {
            meta.repository = Some(format!(
                "https://github.com/garysomerhalder/{}",
                dir_name
            ));
        }
    }

    meta
}

fn parse_cargo_toml(content: &str, meta: &mut ProjectMetadata) {
    let table: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return,
    };

    // Check for workspace
    if let Some(workspace) = table.get("workspace") {
        meta.is_workspace = true;
        meta.project_type = ProjectType::RustWorkspace;

        if let Some(members) = workspace.get("members").and_then(|m| m.as_array()) {
            meta.workspace_members = members
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }

        // Workspace-level package metadata
        if let Some(pkg) = workspace.get("package") {
            extract_package_fields(pkg, meta);
        }
    }

    // Package-level metadata
    if let Some(pkg) = table.get("package") {
        extract_package_fields(pkg, meta);

        // Binary name from [[bin]]
        if let Some(name) = pkg.get("name").and_then(|n| n.as_str()) {
            meta.binary_name = Some(name.to_string());
        }
    }

    // Check [[bin]] sections
    if let Some(bins) = table.get("bin").and_then(|b| b.as_array()) {
        if let Some(first) = bins.first() {
            if let Some(name) = first.get("name").and_then(|n| n.as_str()) {
                meta.binary_name = Some(name.to_string());
            }
        }
    }

    // Detect Rust flavor from dependencies
    let deps = table
        .get("dependencies")
        .and_then(|d| d.as_table())
        .cloned()
        .unwrap_or_default();

    let has_vulcan = deps.contains_key("vulcan")
        || deps.contains_key("vulcan-mcp-sdk");
    let has_clap = deps.contains_key("clap");

    meta.rust_flavor = Some(if has_vulcan {
        RustFlavor::McpServer
    } else if has_clap {
        RustFlavor::Cli
    } else if table.get("lib").is_some() {
        RustFlavor::Library
    } else {
        RustFlavor::Binary
    });

    // Also check workspace dependencies for flavor detection
    if meta.rust_flavor == Some(RustFlavor::Binary) {
        if let Some(workspace) = table.get("workspace") {
            if let Some(wdeps) = workspace.get("dependencies").and_then(|d| d.as_table()) {
                if wdeps.contains_key("vulcan") || wdeps.contains_key("vulcan-mcp-sdk") {
                    meta.rust_flavor = Some(RustFlavor::McpServer);
                } else if wdeps.contains_key("clap") {
                    meta.rust_flavor = Some(RustFlavor::Cli);
                }
            }
        }
    }

    // Path dependencies
    for (name, val) in &deps {
        if let Some(tbl) = val.as_table() {
            if tbl.contains_key("path") {
                meta.path_dependencies.push(name.clone());
            }
        }
        // Inline table: dep = { path = "../..." }
        if let Some(path) = val
            .as_table()
            .and_then(|t| t.get("path"))
            .and_then(|p| p.as_str())
        {
            if path.contains("..") && !meta.path_dependencies.contains(name) {
                meta.path_dependencies.push(name.clone());
            }
        }
    }
}

fn extract_package_fields(pkg: &toml::Value, meta: &mut ProjectMetadata) {
    if let Some(name) = pkg.get("name").and_then(|n| n.as_str()) {
        if meta.name.is_empty() {
            meta.name = name.to_string();
        }
    }
    if let Some(desc) = pkg.get("description").and_then(|d| d.as_str()) {
        if meta.description.is_empty() {
            meta.description = desc.to_string();
        }
    }
    if let Some(ver) = pkg.get("version").and_then(|v| v.as_str()) {
        meta.version = ver.to_string();
    }
    if let Some(license) = pkg.get("license").and_then(|l| l.as_str()) {
        meta.license = license.to_string();
    }
    if let Some(repo) = pkg.get("repository").and_then(|r| r.as_str()) {
        meta.repository = Some(repo.to_string());
    }
    if let Some(rv) = pkg.get("rust-version").and_then(|r| r.as_str()) {
        meta.rust_version = Some(rv.to_string());
    }
    if let Some(authors) = pkg.get("authors").and_then(|a| a.as_array()) {
        meta.authors = authors
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
}

fn parse_package_json(content: &str, meta: &mut ProjectMetadata) {
    let val: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return,
    };
    if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
        meta.name = name.to_string();
    }
    if let Some(desc) = val.get("description").and_then(|d| d.as_str()) {
        meta.description = desc.to_string();
    }
    if let Some(ver) = val.get("version").and_then(|v| v.as_str()) {
        meta.version = ver.to_string();
    }
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// Audit a repo for missing standard files.
pub fn audit(repo: &Path) -> AuditResult {
    let metadata = extract_metadata(repo);

    let standard_files = match metadata.project_type {
        ProjectType::RustBinary | ProjectType::RustWorkspace | ProjectType::Mixed => {
            StandardFile::all_rust()
        }
        _ => StandardFile::all_generic(),
    };

    let mut missing = Vec::new();
    let mut existing = Vec::new();

    for file in standard_files {
        let path = repo.join(file.path());
        if file == StandardFile::DocsDir {
            if path.is_dir() {
                existing.push(file);
            } else {
                missing.push(file);
            }
        } else if path.exists() {
            existing.push(file);
        } else {
            missing.push(file);
        }
    }

    AuditResult {
        repo_path: repo.to_path_buf(),
        metadata,
        missing,
        existing,
    }
}

// ---------------------------------------------------------------------------
// File generation
// ---------------------------------------------------------------------------

/// Generate a standard file's content from templates.
pub fn generate_content(file: StandardFile, meta: &ProjectMetadata) -> String {
    match file {
        StandardFile::Readme => gen_readme(meta),
        StandardFile::ClaudeMd => gen_claude_md(meta),
        StandardFile::Changelog => gen_changelog(meta),
        StandardFile::License => gen_license(meta),
        StandardFile::BuildingMd => gen_building_md(meta),
        StandardFile::SecurityMd => gen_security_md(meta),
        StandardFile::Editorconfig => gen_editorconfig(),
        StandardFile::Gitattributes => gen_gitattributes(),
        StandardFile::Gitignore => gen_gitignore(meta),
        StandardFile::RustfmtToml => gen_rustfmt_toml(),
        StandardFile::DocsDir => String::new(), // handled specially
    }
}

/// Initialize missing files in a repo. Returns what was created/skipped.
pub fn init_repo(repo: &Path, force: bool) -> InitResult {
    let audit_result = audit(repo);
    let files_to_create = if force {
        let mut all = audit_result.missing.clone();
        all.extend(audit_result.existing.iter().copied());
        all
    } else {
        audit_result.missing.clone()
    };

    let skipped: Vec<StandardFile> = if force {
        Vec::new()
    } else {
        audit_result.existing.clone()
    };

    let mut created = Vec::new();
    let mut errors = Vec::new();

    for file in &files_to_create {
        if *file == StandardFile::DocsDir {
            match create_docs_dir(repo) {
                Ok(()) => created.push(*file),
                Err(e) => errors.push((*file, e.to_string())),
            }
            continue;
        }

        let content = generate_content(*file, &audit_result.metadata);
        let path = repo.join(file.path());

        match fs::write(&path, &content) {
            Ok(()) => created.push(*file),
            Err(e) => errors.push((*file, e.to_string())),
        }
    }

    InitResult {
        repo_path: repo.to_path_buf(),
        created,
        skipped,
        errors,
    }
}

fn create_docs_dir(repo: &Path) -> std::io::Result<()> {
    let docs = repo.join("docs");
    let subdirs = [
        "adr",
        "architecture",
        "guides",
        "runbooks",
        "testing",
        "archive",
    ];

    for sub in &subdirs {
        let dir = docs.join(sub);
        fs::create_dir_all(&dir)?;
        let gitkeep = dir.join(".gitkeep");
        if !gitkeep.exists() {
            fs::write(&gitkeep, "")?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Repo discovery (batch mode)
// ---------------------------------------------------------------------------

/// Well-known repos that don't match the suffix patterns
const EXTRA_REPOS: &[&str] = &[
    "sentinel",
    "sentinel-launcher",
    "vulcan-mcp-sdk-rust",
    "mcp-router-rust",
    "claude-code-marketplace",
];

/// Discover all repos under ~/Documents/GitHub/ that match our patterns.
pub fn discover_repos() -> Vec<PathBuf> {
    let gh_dir = match dirs::home_dir() {
        Some(h) => h.join("Documents").join("GitHub"),
        None => return Vec::new(),
    };

    let mut repos = Vec::new();

    let entries = match fs::read_dir(&gh_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.filter_map(|e| e.ok()) {
        if !entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();

        let is_match = name.ends_with("-mcp-rust")
            || name.ends_with("-cli-rust")
            || EXTRA_REPOS.contains(&name.as_str());

        if is_match {
            // Verify it's actually a git repo
            let path = entry.path();
            if path.join(".git").exists() || path.join("Cargo.toml").exists() {
                repos.push(path);
            }
        }
    }

    repos.sort();
    repos
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

fn gen_readme(meta: &ProjectMetadata) -> String {
    let title = &meta.name;
    let desc = if meta.description.is_empty() {
        format!("A Rust project.")
    } else {
        meta.description.clone()
    };

    let flavor_line = match &meta.rust_flavor {
        Some(RustFlavor::McpServer) => {
            let product = title.trim_end_matches("-mcp");
            format!(
                "\nMCP server for {} — built with [Vulcan SDK](https://github.com/garysomerhalder/vulcan-mcp-sdk-rust), wrapped by `mcp-router` for hot-reload.\n",
                product
            )
        }
        Some(RustFlavor::Cli) => {
            let product = title.trim_end_matches("-cli-rs").trim_end_matches("-cli");
            format!("\nCommand-line tool for {}.\n", product)
        }
        _ => String::new(),
    };

    let workspace_section = if meta.is_workspace && !meta.workspace_members.is_empty() {
        let members: Vec<String> = meta
            .workspace_members
            .iter()
            .map(|m| format!("- `{}`", m))
            .collect();
        format!(
            "\n## Workspace Members\n\n{}\n",
            members.join("\n")
        )
    } else {
        String::new()
    };

    let install_section = match &meta.rust_flavor {
        Some(RustFlavor::McpServer) => format!(
            r#"## Installation

```bash
cargo install --path .
```

### Register with Claude Code

```bash
claude mcp add {} -- mcp-router --single {}
```
"#,
            title.trim_end_matches("-mcp"),
            title,
        ),
        Some(RustFlavor::Cli) => format!(
            r#"## Installation

```bash
cargo install --path .
```
"#
        ),
        _ => format!(
            r#"## Installation

```bash
cargo build --release
```
"#
        ),
    };

    format!(
        r#"# {title}

> {desc}
{flavor_line}
{install_section}{workspace_section}
## Usage

```bash
{binary} --help
```

## Development

```bash
cargo build              # Debug build
cargo test               # Run tests
cargo clippy             # Lint
cargo fmt --check        # Check formatting
```

## License

{license}
"#,
        title = title,
        desc = desc,
        flavor_line = flavor_line,
        install_section = install_section,
        workspace_section = workspace_section,
        binary = meta.binary_name.as_deref().unwrap_or(&meta.name),
        license = meta.license,
    )
}

fn gen_claude_md(meta: &ProjectMetadata) -> String {
    let mut sections = Vec::new();

    sections.push(format!(
        "# CLAUDE.md\n\nThis file provides guidance to Claude Code (claude.ai/code) when working with code in this repository."
    ));

    // Build commands
    let build_cmds = match &meta.rust_flavor {
        Some(RustFlavor::McpServer) => format!(
            r#"## Build Commands

```bash
cargo build --release    # Build optimized binary
cargo test               # Run all tests
cargo clippy             # Lint
cargo fmt --check        # Check formatting
```"#
        ),
        Some(RustFlavor::Cli) => format!(
            r#"## Build Commands

```bash
cargo build --release    # Build optimized binary
cargo test               # Run all tests
cargo clippy             # Lint
cargo fmt --check        # Check formatting
cargo install --path .   # Install to ~/.cargo/bin/
```"#
        ),
        _ => format!(
            r#"## Build Commands

```bash
cargo build --release    # Build optimized binary
cargo test               # Run all tests
cargo clippy             # Lint
cargo fmt --check        # Check formatting
```"#
        ),
    };
    sections.push(build_cmds);

    // Architecture
    if meta.is_workspace && !meta.workspace_members.is_empty() {
        let members: Vec<String> = meta
            .workspace_members
            .iter()
            .map(|m| format!("- `{}`", m))
            .collect();
        sections.push(format!(
            "## Architecture\n\nRust workspace with {} crates:\n\n{}",
            meta.workspace_members.len(),
            members.join("\n")
        ));
    }

    // Key dependencies
    if !meta.path_dependencies.is_empty() {
        let deps: Vec<String> = meta
            .path_dependencies
            .iter()
            .map(|d| format!("- `{}` (local path dependency)", d))
            .collect();
        sections.push(format!(
            "## Key Dependencies\n\n{}\n\nThese are local path dependencies — the repos must be cloned as siblings.",
            deps.join("\n")
        ));
    }

    // MCP-specific notes
    if meta.rust_flavor == Some(RustFlavor::McpServer) {
        sections.push(format!(
            r#"## MCP Server

This is a Vulcan SDK MCP server. Key patterns:
- Tools are defined with `#[tool]` proc macro
- Tool handlers use `#[tool_handler]`
- Router uses `#[tool_router]`
- Binary is wrapped by `mcp-router --single {}` for hot-reload"#,
            meta.binary_name.as_deref().unwrap_or(&meta.name)
        ));
    }

    sections.join("\n\n")
}

fn gen_changelog(meta: &ProjectMetadata) -> String {
    format!(
        r#"# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

## [{version}] - {date}

### Added
- Initial release

<!-- generated by sentinel init -->
"#,
        version = meta.version,
        date = chrono::Utc::now().format("%Y-%m-%d"),
    )
}

fn gen_license(meta: &ProjectMetadata) -> String {
    let year = chrono::Utc::now().format("%Y");
    let author = meta
        .authors
        .first()
        .map(|a| a.as_str())
        .unwrap_or("Gary Somerhalder");

    format!(
        r#"MIT License

Copyright (c) {year} {author}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"#,
        year = year,
        author = author,
    )
}

fn gen_building_md(meta: &ProjectMetadata) -> String {
    let rust_version = meta
        .rust_version
        .as_deref()
        .unwrap_or("1.87+");

    let path_deps_note = if meta.path_dependencies.is_empty() {
        String::new()
    } else {
        let deps: Vec<String> = meta
            .path_dependencies
            .iter()
            .map(|d| format!("- `{}`", d))
            .collect();
        format!(
            "\n### Local Dependencies\n\nThis project has path dependencies that must be cloned as sibling directories:\n\n{}\n",
            deps.join("\n")
        )
    };

    format!(
        r#"# Building

## Prerequisites

- [Rust](https://rustup.rs/) {rust_version} (rustc, cargo)
{path_deps_note}
## Build

```bash
cargo build --release
```

The binary will be at `target/release/{binary}`.

## Test

```bash
cargo test
```

## Install

```bash
cargo install --path .
```
"#,
        rust_version = rust_version,
        binary = meta.binary_name.as_deref().unwrap_or(&meta.name),
        path_deps_note = path_deps_note,
    )
}

fn gen_security_md(meta: &ProjectMetadata) -> String {
    let repo_url = meta
        .repository
        .as_deref()
        .unwrap_or("https://github.com/garysomerhalder");

    format!(
        r#"# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability, please report it responsibly:

1. **Do NOT** open a public GitHub issue
2. Use [GitHub Security Advisories]({repo_url}/security/advisories/new) to report privately
3. Or email: security@garysomerhalder.com

## Response Timeline

- **Acknowledgment**: Within 48 hours
- **Assessment**: Within 1 week
- **Fix**: Depends on severity, typically within 2 weeks for critical issues
"#,
        repo_url = repo_url,
    )
}

fn gen_editorconfig() -> String {
    r#"root = true

[*]
charset = utf-8
end_of_line = lf
insert_final_newline = true
trim_trailing_whitespace = true
indent_style = space
indent_size = 4

[*.{md,yml,yaml,toml,json}]
indent_size = 2
"#
    .to_string()
}

fn gen_gitattributes() -> String {
    r#"* text=auto eol=lf
*.rs text diff=rust
*.toml text diff=toml
*.md text diff=markdown
*.lock -diff
*.exe binary
*.dll binary
*.so binary
*.dylib binary
"#
    .to_string()
}

fn gen_gitignore(meta: &ProjectMetadata) -> String {
    let mut lines = vec![
        "/target",
        "**/*.rs.bk",
        "*.pdb",
        "",
        "# Editor",
        ".vscode/",
        ".idea/",
        "*.swp",
        "*~",
        "",
        "# OS",
        ".DS_Store",
        "Thumbs.db",
    ];

    // Node additions for mixed projects
    if meta.project_type == ProjectType::Node || meta.project_type == ProjectType::Mixed {
        lines.extend_from_slice(&["", "# Node", "node_modules/", "dist/", ".env", ".env.local"]);
    }

    lines.join("\n") + "\n"
}

fn gen_rustfmt_toml() -> String {
    r#"newline_style = "Unix"
group_imports = "StdExternalCrate"
imports_granularity = "Crate"
max_width = 100
"#
    .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_metadata_rust_single() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"[package]
name = "test-mcp"
version = "0.1.0"
description = "A test MCP server"
license = "MIT"

[dependencies]
vulcan = { path = "../vulcan-mcp-sdk-rust/crates/vulcan" }
"#,
        )
        .unwrap();

        let meta = extract_metadata(dir.path());
        assert_eq!(meta.name, "test-mcp");
        assert_eq!(meta.description, "A test MCP server");
        assert_eq!(meta.rust_flavor, Some(RustFlavor::McpServer));
        assert!(meta.path_dependencies.contains(&"vulcan".to_string()));
    }

    #[test]
    fn test_extract_metadata_rust_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/core", "crates/cli"]

[workspace.package]
version = "0.3.0"
"#,
        )
        .unwrap();

        let meta = extract_metadata(dir.path());
        assert_eq!(meta.project_type, ProjectType::RustWorkspace);
        assert!(meta.is_workspace);
        assert_eq!(meta.workspace_members, vec!["crates/core", "crates/cli"]);
    }

    #[test]
    fn test_extract_metadata_cli() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            r#"[package]
name = "doppler-cli-rs"
version = "1.0.0"
description = "Doppler CLI"

[dependencies]
clap = { version = "4", features = ["derive"] }
"#,
        )
        .unwrap();

        let meta = extract_metadata(dir.path());
        assert_eq!(meta.rust_flavor, Some(RustFlavor::Cli));
    }

    #[test]
    fn test_extract_metadata_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let meta = extract_metadata(dir.path());
        assert_eq!(meta.project_type, ProjectType::Unknown);
    }

    #[test]
    fn test_audit_all_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nversion = \"0.1.0\"\n").unwrap();

        let result = audit(dir.path());
        assert!(result.missing.contains(&StandardFile::Readme));
        assert!(result.missing.contains(&StandardFile::ClaudeMd));
        assert!(result.missing.contains(&StandardFile::License));
        assert!(result.missing.contains(&StandardFile::RustfmtToml));
    }

    #[test]
    fn test_audit_some_existing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(dir.path().join("README.md"), "# Test").unwrap();
        fs::write(dir.path().join("LICENSE"), "MIT").unwrap();

        let result = audit(dir.path());
        assert!(result.existing.contains(&StandardFile::Readme));
        assert!(result.existing.contains(&StandardFile::License));
        assert!(result.missing.contains(&StandardFile::ClaudeMd));
    }

    #[test]
    fn test_init_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-tool\"\nversion = \"0.2.0\"\ndescription = \"A cool tool\"\n",
        )
        .unwrap();

        let result = init_repo(dir.path(), false);
        assert!(!result.created.is_empty());
        assert!(result.errors.is_empty());

        // Verify files exist
        assert!(dir.path().join("README.md").exists());
        assert!(dir.path().join("CLAUDE.md").exists());
        assert!(dir.path().join("CHANGELOG.md").exists());
        assert!(dir.path().join("LICENSE").exists());
        assert!(dir.path().join("BUILDING.md").exists());
        assert!(dir.path().join("SECURITY.md").exists());
        assert!(dir.path().join(".editorconfig").exists());
        assert!(dir.path().join(".gitattributes").exists());
        assert!(dir.path().join(".gitignore").exists());
        assert!(dir.path().join("rustfmt.toml").exists());
        assert!(dir.path().join("docs").join("adr").join(".gitkeep").exists());
    }

    #[test]
    fn test_init_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(dir.path().join("README.md"), "# My Custom README").unwrap();

        let result = init_repo(dir.path(), false);
        assert!(result.skipped.contains(&StandardFile::Readme));
        assert!(!result.created.contains(&StandardFile::Readme));

        // Verify custom README was NOT overwritten
        let content = fs::read_to_string(dir.path().join("README.md")).unwrap();
        assert_eq!(content, "# My Custom README");
    }

    #[test]
    fn test_init_force_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(dir.path().join("README.md"), "old content").unwrap();

        let result = init_repo(dir.path(), true);
        assert!(result.created.contains(&StandardFile::Readme));

        let content = fs::read_to_string(dir.path().join("README.md")).unwrap();
        assert!(content.contains("# test"));
    }

    #[test]
    fn test_gen_readme_mcp() {
        let meta = ProjectMetadata {
            name: "steel-mcp".to_string(),
            description: "Cloud browser automation".to_string(),
            rust_flavor: Some(RustFlavor::McpServer),
            binary_name: Some("steel-mcp".to_string()),
            ..Default::default()
        };

        let readme = gen_readme(&meta);
        assert!(readme.contains("# steel-mcp"));
        assert!(readme.contains("MCP server"));
        assert!(readme.contains("Vulcan SDK"));
        assert!(readme.contains("mcp-router"));
    }

    #[test]
    fn test_gen_claude_md_workspace() {
        let meta = ProjectMetadata {
            name: "sentinel".to_string(),
            is_workspace: true,
            workspace_members: vec!["crates/domain".into(), "crates/cli".into()],
            rust_flavor: Some(RustFlavor::Binary),
            ..Default::default()
        };

        let claude = gen_claude_md(&meta);
        assert!(claude.contains("workspace with 2 crates"));
        assert!(claude.contains("`crates/domain`"));
    }

    #[test]
    fn test_gen_editorconfig() {
        let content = gen_editorconfig();
        assert!(content.contains("root = true"));
        assert!(content.contains("end_of_line = lf"));
        assert!(content.contains("indent_size = 4"));
    }

    #[test]
    fn test_gen_gitattributes() {
        let content = gen_gitattributes();
        assert!(content.contains("*.rs text diff=rust"));
        assert!(content.contains("*.exe binary"));
    }

    #[test]
    fn test_gen_gitignore_rust() {
        let meta = ProjectMetadata {
            project_type: ProjectType::RustBinary,
            ..Default::default()
        };
        let content = gen_gitignore(&meta);
        assert!(content.contains("/target"));
        assert!(!content.contains("node_modules"));
    }

    #[test]
    fn test_gen_gitignore_mixed() {
        let meta = ProjectMetadata {
            project_type: ProjectType::Mixed,
            ..Default::default()
        };
        let content = gen_gitignore(&meta);
        assert!(content.contains("/target"));
        assert!(content.contains("node_modules"));
    }

    #[test]
    fn test_gen_license() {
        let meta = ProjectMetadata::default();
        let license = gen_license(&meta);
        assert!(license.contains("MIT License"));
        assert!(license.contains("Gary Somerhalder"));
    }

    #[test]
    fn test_gen_building_md_with_path_deps() {
        let meta = ProjectMetadata {
            name: "steel-mcp".to_string(),
            binary_name: Some("steel-mcp".to_string()),
            path_dependencies: vec!["vulcan".to_string()],
            ..Default::default()
        };
        let content = gen_building_md(&meta);
        assert!(content.contains("Local Dependencies"));
        assert!(content.contains("`vulcan`"));
        assert!(content.contains("sibling directories"));
    }

    #[test]
    fn test_docs_dir_creation() {
        let dir = tempfile::tempdir().unwrap();
        create_docs_dir(dir.path()).unwrap();
        assert!(dir.path().join("docs").join("adr").join(".gitkeep").exists());
        assert!(dir.path().join("docs").join("architecture").join(".gitkeep").exists());
        assert!(dir.path().join("docs").join("guides").join(".gitkeep").exists());
        assert!(dir.path().join("docs").join("runbooks").join(".gitkeep").exists());
        assert!(dir.path().join("docs").join("testing").join(".gitkeep").exists());
        assert!(dir.path().join("docs").join("archive").join(".gitkeep").exists());
    }
}
