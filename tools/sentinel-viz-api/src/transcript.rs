use std::path::{Path, PathBuf};

/// Locate the conversation transcript JSONL for a session, across
/// both Claude homes. Mirrors `find_transcript()` in viz_server.py.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let name = format!("{session_id}.jsonl");
    let home = dirs::home_dir()?;
    let roots = [home.join(".claude/projects"), home.join(".claude-sentinel/projects")];
    for root in roots {
        if !root.exists() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for entry in rd.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let cand = entry.path().join(&name);
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

pub fn trim(s: &str, n: usize) -> String {
    let cleaned: String = s.replace('\n', " ").trim().to_string();
    if cleaned.chars().count() <= n {
        cleaned
    } else {
        let truncated: String = cleaned.chars().take(n).collect();
        format!("{truncated}…")
    }
}

#[allow(dead_code)]
pub fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let Ok(file) = std::fs::read_to_string(path) else { return vec![] };
    file.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                None
            } else {
                serde_json::from_str(l).ok()
            }
        })
        .collect()
}
