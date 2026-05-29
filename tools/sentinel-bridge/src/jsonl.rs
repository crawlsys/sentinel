//! JSONL tailer with byte-offset state.
//!
//! Mirrors the Python bridge's "track offset per file, re-read on
//! poll, handle rotation" behaviour. On rotation (file truncates or
//! shrinks below the remembered offset), we reset to zero and
//! re-read from the top so the rotated-in content isn't lost.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OffsetState {
    /// path → byte offset already consumed.
    pub offsets: HashMap<String, u64>,
}

impl OffsetState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let parsed: Self = serde_json::from_str(&raw).unwrap_or_default();
        Ok(parsed)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }
}

/// Read newline-delimited JSON objects from `path`, starting at the
/// remembered offset. Returns (records, new_offset). Lines that fail
/// to parse are silently skipped (matches the Python behaviour — a
/// corrupted line shouldn't kill ingestion).
pub fn read_new(path: &Path, offset: u64) -> Result<(Vec<Value>, u64)> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok((vec![], offset));
    };
    let size = meta.len();
    let start = if size < offset { 0 } else { offset };
    if size <= start {
        return Ok((vec![], size));
    }
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::new(f);
    reader.seek(SeekFrom::Start(start))?;
    let mut out = Vec::new();
    let mut new_offset = start;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        new_offset += n as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => out.push(v),
            Err(_) => continue,
        }
    }
    Ok((out, new_offset))
}

/// Read the entire file once. Used by shims that operate on
/// always-rewritten JSON arrays (gemini) rather than append-only
/// JSONLs — only the `extra-harnesses` feature wires those in.
#[allow(dead_code)]
pub fn read_full_array(path: &Path) -> Result<Vec<Value>> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)?;
    match parsed {
        Value::Array(arr) => Ok(arr),
        _ => Ok(vec![]),
    }
}

/// Glob expansion that returns paths sorted by mtime ascending.
/// Newest files sort last so iteration order matches the activegraph
/// bridge's "newest data sorts last" rule.
pub fn glob_by_mtime(pattern: &str) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<(PathBuf, std::time::SystemTime)> = vec![];
    for entry in glob::glob(pattern).with_context(|| format!("bad glob {pattern}"))? {
        let path = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Ok(meta) = fs::metadata(&path) {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            paths.push((path, mtime));
        }
    }
    paths.sort_by_key(|p| p.1);
    Ok(paths.into_iter().map(|(p, _)| p).collect())
}
