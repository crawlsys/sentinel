use std::path::{Path, PathBuf};

/// Resolve the home directory the Claude transcript roots live under.
/// Honors the `SENTINEL_VIZ_HOME` override first (used by tests and any
/// non-standard layout), then falls back to the OS home. This avoids
/// relying on `$HOME`, which `dirs::home_dir()` ignores on Windows (it
/// reads `USERPROFILE` / the Known Folder API), so a `HOME`-only override
/// silently does nothing there.
fn viz_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("SENTINEL_VIZ_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir()
}

/// Locate the conversation transcript JSONL for a session, across
/// both Claude homes. Mirrors `find_transcript()` in `viz_server.py`.
///
/// WORKSTREAM: claude-code — both `~/.claude/projects/` and
/// `~/.claude-sentinel/projects/` are owned by Claude Code itself
/// (and the sandboxed Claude Code respectively). The viz crate only
/// reads JSONL files; never writes. If Claude Code ever changes the
/// transcript layout, this function is the touchpoint to update.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let name = format!("{session_id}.jsonl");
    let home = viz_home()?;
    let roots = [home.join(".claude/projects"), home.join(".claude-sentinel/projects")];
    for root in roots {
        if !root.exists() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for entry in rd.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
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
