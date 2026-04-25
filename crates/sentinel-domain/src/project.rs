//! Project initialization domain types
//!
//! Types for auditing and generating standard project files
//! across repositories.

use std::fmt;
use std::path::PathBuf;

/// Detected project type based on manifest files
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectType {
    /// Single Cargo.toml with [package]
    RustBinary,
    /// Cargo.toml with [workspace]
    RustWorkspace,
    /// package.json present
    Node,
    /// Both Cargo.toml and package.json
    Mixed,
    /// No recognized manifest
    Unknown,
}

impl fmt::Display for ProjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RustBinary => write!(f, "Rust Binary"),
            Self::RustWorkspace => write!(f, "Rust Workspace"),
            Self::Node => write!(f, "Node"),
            Self::Mixed => write!(f, "Mixed (Rust + Node)"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Flavor of a Rust project (affects README/CLAUDE.md templates)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustFlavor {
    /// MCP server (depends on vulcan)
    McpServer,
    /// CLI tool (depends on clap)
    Cli,
    /// Library crate
    Library,
    /// Generic binary
    Binary,
}

impl fmt::Display for RustFlavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::McpServer => write!(f, "MCP Server"),
            Self::Cli => write!(f, "CLI"),
            Self::Library => write!(f, "Library"),
            Self::Binary => write!(f, "Binary"),
        }
    }
}

/// Metadata extracted from a project's manifest files
#[derive(Debug, Clone)]
pub struct ProjectMetadata {
    pub name: String,
    pub description: String,
    pub version: String,
    pub authors: Vec<String>,
    pub license: String,
    pub repository: Option<String>,
    pub project_type: ProjectType,
    pub rust_flavor: Option<RustFlavor>,
    pub binary_name: Option<String>,
    pub is_workspace: bool,
    pub workspace_members: Vec<String>,
    pub path_dependencies: Vec<String>,
    pub rust_version: Option<String>,
}

impl Default for ProjectMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            version: "0.1.0".to_string(),
            authors: Vec::new(),
            license: "MIT".to_string(),
            repository: None,
            project_type: ProjectType::Unknown,
            rust_flavor: None,
            binary_name: None,
            is_workspace: false,
            workspace_members: Vec::new(),
            path_dependencies: Vec::new(),
            rust_version: None,
        }
    }
}

/// Standard files that every repo should have
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardFile {
    Readme,
    ClaudeMd,
    Changelog,
    License,
    BuildingMd,
    SecurityMd,
    Editorconfig,
    Gitattributes,
    Gitignore,
    RustfmtToml,
    DocsDir,
}

impl StandardFile {
    /// Filesystem path relative to repo root
    pub const fn path(&self) -> &'static str {
        match self {
            Self::Readme => "README.md",
            Self::ClaudeMd => "CLAUDE.md",
            Self::Changelog => "CHANGELOG.md",
            Self::License => "LICENSE",
            Self::BuildingMd => "BUILDING.md",
            Self::SecurityMd => "SECURITY.md",
            Self::Editorconfig => ".editorconfig",
            Self::Gitattributes => ".gitattributes",
            Self::Gitignore => ".gitignore",
            Self::RustfmtToml => "rustfmt.toml",
            Self::DocsDir => "docs",
        }
    }

    /// Whether this file is Rust-specific
    pub const fn is_rust_only(&self) -> bool {
        matches!(self, Self::RustfmtToml)
    }

    /// All standard files for a Rust project
    pub fn all_rust() -> Vec<Self> {
        vec![
            Self::Readme,
            Self::ClaudeMd,
            Self::Changelog,
            Self::License,
            Self::BuildingMd,
            Self::SecurityMd,
            Self::Editorconfig,
            Self::Gitattributes,
            Self::Gitignore,
            Self::RustfmtToml,
            Self::DocsDir,
        ]
    }

    /// All standard files for a non-Rust project
    pub fn all_generic() -> Vec<Self> {
        vec![
            Self::Readme,
            Self::ClaudeMd,
            Self::Changelog,
            Self::License,
            Self::BuildingMd,
            Self::SecurityMd,
            Self::Editorconfig,
            Self::Gitattributes,
            Self::Gitignore,
            Self::DocsDir,
        ]
    }
}

impl fmt::Display for StandardFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path())
    }
}

/// Result of auditing a single repo
#[derive(Debug, Clone)]
pub struct AuditResult {
    pub repo_path: PathBuf,
    pub metadata: ProjectMetadata,
    pub missing: Vec<StandardFile>,
    pub existing: Vec<StandardFile>,
}

/// Result of initializing files in a single repo
#[derive(Debug, Clone)]
pub struct InitResult {
    pub repo_path: PathBuf,
    pub created: Vec<StandardFile>,
    pub skipped: Vec<StandardFile>,
    pub errors: Vec<(StandardFile, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_file_paths() {
        assert_eq!(StandardFile::Readme.path(), "README.md");
        assert_eq!(StandardFile::Editorconfig.path(), ".editorconfig");
        assert_eq!(StandardFile::DocsDir.path(), "docs");
    }

    #[test]
    fn test_rust_only() {
        assert!(StandardFile::RustfmtToml.is_rust_only());
        assert!(!StandardFile::Readme.is_rust_only());
    }

    #[test]
    fn test_all_rust_includes_rustfmt() {
        let all = StandardFile::all_rust();
        assert!(all.contains(&StandardFile::RustfmtToml));
        assert_eq!(all.len(), 11);
    }

    #[test]
    fn test_all_generic_excludes_rustfmt() {
        let all = StandardFile::all_generic();
        assert!(!all.contains(&StandardFile::RustfmtToml));
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn test_project_type_display() {
        assert_eq!(ProjectType::RustWorkspace.to_string(), "Rust Workspace");
        assert_eq!(RustFlavor::McpServer.to_string(), "MCP Server");
    }

    #[test]
    fn test_default_metadata() {
        let meta = ProjectMetadata::default();
        assert_eq!(meta.license, "MIT");
        assert_eq!(meta.version, "0.1.0");
        assert!(meta.authors.is_empty());
    }
}
