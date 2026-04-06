//! Real filesystem adapter — implements FileSystemPort.
//!
//! Thin delegation to std::fs + dirs. Exists so hooks can be tested
//! with a mock filesystem that doesn't touch real disk.

use anyhow::{Context, Result};
use sentinel_application::hooks::FileSystemPort;
use std::path::{Path, PathBuf};

/// Infrastructure adapter implementing `FileSystemPort` via real std::fs.
pub struct RealFileSystem;

impl FileSystemPort for RealFileSystem {
    fn home_dir(&self) -> Option<PathBuf> {
        dirs::home_dir()
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

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn metadata(&self, path: &Path) -> Result<std::fs::Metadata> {
        std::fs::metadata(path).with_context(|| format!("metadata {}", path.display()))
    }
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
}
