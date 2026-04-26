//! Memory Extract Hook — sync flat-file memories to Qdrant
//!
//! Fires on Stop. Detects memory files that changed since the last sync
//! (tracked via a state file, not a time window) and upserts them to Qdrant.
//!
//! Claude decides what to remember → writes .md file → this hook syncs to Qdrant.
//!
//! **Periodic session re-index:** Every 50 tool calls, indexes the last ~10
//! substantive exchanges to keep long-running sessions searchable.

use super::{FileSystemPort, MemoryMcpPort, VectorPoint, VectorStorePort};
use sentinel_domain::constants;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Last-synced state tracking (replaces 30s time window)
// ---------------------------------------------------------------------------

/// State file: maps file path -> last synced mtime (as unix timestamp)
fn sync_state_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("memory-sync-state.json")
    })
}

fn load_sync_state(fs: &dyn FileSystemPort) -> HashMap<String, u64> {
    let path = match sync_state_path(fs) {
        Some(p) => p,
        None => return HashMap::new(),
    };
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_sync_state(fs: &dyn FileSystemPort, state: &HashMap<String, u64>) {
    let path = match sync_state_path(fs) {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(state) {
        let _ = fs.write(&path, json.as_bytes());
    }
}

/// Get mtime as unix timestamp for a file
fn file_mtime(fs: &dyn FileSystemPort, path: &std::path::Path) -> Option<u64> {
    fs.metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Find memory files that have changed since last sync
fn find_unsynced_memories(fs: &dyn FileSystemPort) -> Vec<PathBuf> {
    let home = match fs.home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let projects_dir = home.join(".claude").join("projects");
    if !fs.is_dir(&projects_dir) {
        return vec![];
    }

    let state = load_sync_state(fs);
    let mut unsynced = Vec::new();

    if let Ok(entries) = fs.read_dir(&projects_dir) {
        for entry in entries {
            let memory_dir = entry.join("memory");
            if !fs.is_dir(&memory_dir) {
                continue;
            }
            if let Ok(files) = fs.read_dir(&memory_dir) {
                for path in files {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !name.ends_with(".md") || name == "MEMORY.md" {
                        continue;
                    }
                    let key = path.to_string_lossy().to_string();
                    let current_mtime = file_mtime(fs, &path).unwrap_or(0);
                    let last_synced = state.get(&key).copied().unwrap_or(0);

                    if current_mtime > last_synced {
                        unsynced.push(path);
                    }
                }
            }
        }
    }

    unsynced
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse frontmatter from a memory file
fn parse_frontmatter(content: &str) -> Option<(String, String, String, String)> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("---")?;
    let frontmatter = &rest[..end];
    let body = rest[end + 3..].trim().to_string();

    let mut name = String::new();
    let mut description = String::new();
    let mut mem_type = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("description:") {
            description = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("type:") {
            mem_type = val.trim().to_string();
        }
    }

    if name.is_empty() && description.is_empty() {
        return None;
    }

    Some((name, description, mem_type, body))
}

// ---------------------------------------------------------------------------
// Memory-engine capture path (memory-mcp memory_capture)
// ---------------------------------------------------------------------------

/// Read a flat-file memory, project it into the Memory engine's
/// subject/predicate/value shape, and submit via `memory_capture` through
/// the MemoryMcpPort. Returns true when the server accepted the write
/// (committed OR reinforced OR amended OR quarantined — anything except
/// dropped).
fn capture_memory_via_mcp(
    fs: &dyn FileSystemPort,
    memory_mcp: &dyn MemoryMcpPort,
    path: &PathBuf,
) -> bool {
    let content = match fs.read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let (name, description, mem_type, body) = match parse_frontmatter(&content) {
        Some(fm) => fm,
        None => return false,
    };

    // Project memory_extract's shape (name/description/memory_type/body)
    // into memory_capture's shape (subject/predicate/value/project). The
    // mapping is lossy-but-principled:
    //   - subject   = name            (the fact's head entity)
    //   - predicate = memory_type     (falls back to "describes")
    //   - value     = description     (+ first 500 chars of body)
    //   - project   = "auto-extract"  (same as legacy default)
    // If either side's schema evolves, revisit here first.
    let subject = if name.is_empty() {
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("unnamed").to_string()
    } else {
        name
    };
    let predicate = if mem_type.is_empty() { "describes".to_string() } else { mem_type };
    let value = if body.is_empty() {
        description
    } else {
        // Cap the value at a reasonable payload size — the MCP input
        // validator imposes 64 KiB, but atoms shouldn't be prose dumps.
        let body_excerpt: String = body.chars().take(500).collect();
        format!("{description}\n\n{body_excerpt}")
    };

    let mut args = serde_json::Map::new();
    args.insert("subject".into(), serde_json::Value::String(subject));
    args.insert("predicate".into(), serde_json::Value::String(predicate));
    args.insert("value".into(), serde_json::Value::String(value));
    args.insert("project".into(), serde_json::Value::String("auto-extract".into()));
    // Tag the qualifier with the source file path so memory_audit can
    // correlate atoms back to the .md they came from.
    let source_path = path.to_string_lossy().to_string();
    args.insert(
        "qualifier".into(),
        serde_json::Value::String(format!("source_file={source_path}")),
    );

    // run_async returns Option's Default — None — on timeout or error.
    let out: Option<serde_json::Value> = super::run_async(async move {
        match memory_mcp.call_tool("memory_capture", args).await {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(error = %e, "memory_capture via port returned error");
                None
            }
        }
    });

    let out = match out {
        Some(v) => v,
        None => {
            warn!(file = %path.display(), "memory_capture returned no payload — treating as failure");
            return false;
        }
    };

    // Response shape: `{ "branch": "written"|"reinforced"|"superseded"|
    //                             "quarantined"|"dropped", "atom_id"?: "..." }`
    // `dropped` (both judges rejected) is still a successful sync — it
    // means the file has been seen and judged; we just don't write an
    // atom. Return true so the sync-state advances past it and we don't
    // re-submit on every cron cycle.
    let branch = out.get("branch").and_then(|v| v.as_str()).unwrap_or("");
    matches!(
        branch,
        "written" | "reinforced" | "superseded" | "quarantined" | "dropped"
    )
}

// ---------------------------------------------------------------------------
// Periodic session re-index — every ~50 tool calls
// ---------------------------------------------------------------------------

const REINDEX_TOOL_CALL_THRESHOLD: u64 = constants::REINDEX_TOOL_CALL_THRESHOLD;
const SESSION_COLLECTION: &str = "claude-sessions";

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SessionIndexState {
    tool_calls_since_index: u64,
    last_indexed_at: Option<String>,
}

fn session_index_state_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("session-index-state.json")
    })
}

fn load_session_index_state(fs: &dyn FileSystemPort) -> SessionIndexState {
    let path = match session_index_state_path(fs) {
        Some(p) => p,
        None => return SessionIndexState::default(),
    };
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return SessionIndexState::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_session_index_state(fs: &dyn FileSystemPort, state: &SessionIndexState) {
    let path = match session_index_state_path(fs) {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = fs.write(&path, json.as_bytes());
    }
}

/// Deterministic UUID from session + chunk index.
fn content_to_uuid(session_id: &str, chunk_index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{session_id}:periodic:{chunk_index}").as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&result[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// Derive project name from cwd (last path component).
fn project_name(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Compute project hash from cwd.
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
}

// `is_substantive_exchange` has moved to `sentinel_domain::exchange`. Re-export
// the symbol via `use` so the call sites below don't need to qualify it.
use sentinel_domain::exchange::is_substantive_exchange;

/// Lightweight session re-index: parse the last ~10 exchanges from the
/// transcript and upsert substantive ones via `VectorStorePort` to the
/// `claude-sessions` collection. The adapter handles HTTP, auth, and the
/// embedding-model selection from its config.
fn periodic_session_index(
    fs: &dyn FileSystemPort,
    vector_store: &dyn VectorStorePort,
    transcript_path: &str,
    session_id: &str,
    cwd: &str,
) {
    let transcript = std::path::Path::new(transcript_path);
    let content = match fs.read_to_string(transcript) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to read transcript for periodic index");
            return;
        }
    };

    // Parse last ~10 user+assistant exchanges from JSONL
    let mut exchanges: Vec<(String, String)> = Vec::new();
    let mut current_user = String::new();
    let mut current_assistant = String::new();
    let mut in_exchange = false;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .or_else(|| val.get("role").and_then(|v| v.as_str()))
            .unwrap_or("");

        let text = extract_text_content(&val);

        match msg_type {
            "human" | "user" => {
                if in_exchange && (!current_user.is_empty() || !current_assistant.is_empty()) {
                    exchanges.push((
                        std::mem::take(&mut current_user),
                        std::mem::take(&mut current_assistant),
                    ));
                }
                current_user = text;
                in_exchange = true;
            }
            "assistant" => {
                if !text.is_empty() {
                    if !current_assistant.is_empty() {
                        current_assistant.push('\n');
                    }
                    current_assistant.push_str(&text);
                }
                in_exchange = true;
            }
            _ => {}
        }
    }

    // Flush final exchange
    if in_exchange && (!current_user.is_empty() || !current_assistant.is_empty()) {
        exchanges.push((current_user, current_assistant));
    }

    // Take only the last 10
    let start = exchanges.len().saturating_sub(10);
    let recent = &exchanges[start..];

    if recent.is_empty() {
        debug!("No exchanges to index periodically");
        return;
    }

    let project = project_name(cwd);
    let proj_hash = project_hash(cwd);
    let now = chrono::Utc::now().to_rfc3339();

    // Build VectorPoints — filter out trivial exchanges. The embedding model
    // is the adapter's responsibility (configured at construction time), so
    // callers just supply the text.
    let points: Vec<VectorPoint> = recent
        .iter()
        .enumerate()
        .filter(|(_, (u, a))| is_substantive_exchange(u, a))
        .map(|(i, (user, assistant))| {
            let id = content_to_uuid(session_id, start + i);
            let combined = format!("User: {user}\nAssistant: {assistant}");
            let embed_text = if combined.len() > 2000 {
                format!("{}...", &combined[..1997])
            } else {
                combined.clone()
            };

            VectorPoint {
                id,
                text: embed_text,
                payload: serde_json::json!({
                    "session_id": session_id,
                    "project": project,
                    "project_hash": proj_hash,
                    "timestamp": now,
                    "chunk_type": "periodic_exchange",
                    "chunk_index": start + i,
                    "content": combined
                }),
            }
        })
        .collect();

    if points.is_empty() {
        debug!("No substantive exchanges to index");
        return;
    }

    let count = points.len();
    super::run_async(async {
        match vector_store.upsert_points(SESSION_COLLECTION, points).await {
            Ok(()) => info!(count, session = session_id, "Periodic session index upserted"),
            Err(e) => warn!(error = %e, "Periodic session index upsert failed"),
        }
    });
}

/// Extract text content from a JSONL message value.
fn extract_text_content(val: &serde_json::Value) -> String {
    let msg_content = val
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| val.get("content"));

    msg_content
        .and_then(|c| match c {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(arr) => {
                let parts: Vec<String> = arr
                    .iter()
                    .filter_map(|item| {
                        if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                            item.get("text").and_then(|v| v.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join("\n"))
                }
            }
            _ => None,
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Main hook entry point
// ---------------------------------------------------------------------------

/// Process Stop — sync changed memory files to Qdrant,
/// and periodically re-index the session transcript.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let fs = ctx.fs;
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // --- Periodic session re-index ---
    let mut index_state = load_session_index_state(fs);
    index_state.tool_calls_since_index += 1;

    if index_state.tool_calls_since_index >= REINDEX_TOOL_CALL_THRESHOLD {
        debug!(
            calls = index_state.tool_calls_since_index,
            "Tool call threshold reached — triggering periodic session index"
        );

        if let (Some(session_id), Some(transcript_path)) =
            (input.session_id.as_deref(), input.transcript_path.as_deref())
        {
            if !session_id.is_empty()
                && !transcript_path.is_empty()
                && fs.exists(std::path::Path::new(transcript_path))
            {
                // Skip silently if no vector store is configured — the hook
                // is best-effort and never blocks the session.
                if let Some(vs) = ctx.vector_store {
                    periodic_session_index(fs, vs, transcript_path, session_id, cwd);
                }
            }
        }

        index_state.tool_calls_since_index = 0;
        index_state.last_indexed_at = Some(chrono::Utc::now().to_rfc3339());
    }

    save_session_index_state(fs, &index_state);

    // --- Memory file sync (state-tracked, replaces 30s window) ---
    let unsynced = find_unsynced_memories(fs);
    if unsynced.is_empty() {
        return HookOutput::allow();
    }

    debug!(count = unsynced.len(), "Found unsynced memory files");

    // Sync each changed file via the Memory engine's `memory_capture` MCP
    // tool — every write goes through the dual-judge gate. No direct Qdrant
    // upsert path; the legacy `upsert_memory` helper and the
    // MEMORY_ENGINE_UNIFIED env flag were removed in the migration that
    // made memory-mcp the only path.
    let mut state = load_sync_state(fs);
    let mut synced = 0;
    for path in &unsynced {
        if capture_memory_via_mcp(fs, ctx.memory_mcp, path) {
            synced += 1;
            let key = path.to_string_lossy().to_string();
            let mtime = file_mtime(fs, path).unwrap_or(0);
            state.insert(key, mtime);
            debug!(file = %path.display(), "Synced memory via memory-mcp");
        }
    }

    if synced > 0 {
        save_sync_state(fs, &state);
        info!(synced, total = unsynced.len(), "Memory files synced to Qdrant");
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = "---\nname: test\ndescription: desc\ntype: feedback\n---\nBody";
        let (name, desc, typ, body) = parse_frontmatter(content).unwrap();
        assert_eq!(name, "test");
        assert_eq!(desc, "desc");
        assert_eq!(typ, "feedback");
        assert_eq!(body, "Body");
    }

    #[test]
    fn test_parse_frontmatter_invalid() {
        assert!(parse_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn test_process_no_recent_files() {
        let input = HookInput {
            cwd: Some("/nonexistent".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_is_substantive_exchange() {
        assert!(!is_substantive_exchange("yes", "ok"));
        assert!(!is_substantive_exchange("y", "done"));
        assert!(is_substantive_exchange(
            "fix the authentication bug in login.rs",
            "I found the issue in the token validation. The JWT expiry check was comparing timestamps in different formats."
        ));
        // Short user + long assistant = ok
        assert!(is_substantive_exchange(
            "fix it",
            &"x".repeat(250)
        ));
        // Trivial user + short assistant = skip
        assert!(!is_substantive_exchange("ok", "Done."));
    }

    #[test]
    fn test_session_index_state_roundtrip() {
        let state = SessionIndexState {
            tool_calls_since_index: 42,
            last_indexed_at: Some("2026-04-04T10:00:00Z".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: SessionIndexState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tool_calls_since_index, 42);
        assert_eq!(
            loaded.last_indexed_at.as_deref(),
            Some("2026-04-04T10:00:00Z")
        );
    }

    #[test]
    fn test_session_index_state_default() {
        let state = SessionIndexState::default();
        assert_eq!(state.tool_calls_since_index, 0);
        assert!(state.last_indexed_at.is_none());
    }

    #[test]
    fn test_content_to_uuid_deterministic() {
        let id1 = content_to_uuid("session-abc", 0);
        let id2 = content_to_uuid("session-abc", 0);
        assert_eq!(id1, id2);

        let id3 = content_to_uuid("session-abc", 1);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_project_name() {
        assert_eq!(project_name("/Users/gary/projects/firefly"), "firefly");
        assert_eq!(
            project_name("C:\\Users\\garys\\Documents\\GitHub\\sentinel"),
            "sentinel"
        );
    }

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_reindex_threshold() {
        assert_eq!(REINDEX_TOOL_CALL_THRESHOLD, 50);
    }

    #[test]
    fn test_extract_text_content_string() {
        let val = serde_json::json!({"content": "hello world"});
        assert_eq!(extract_text_content(&val), "hello world");
    }

    #[test]
    fn test_extract_text_content_array() {
        let val = serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "url": "..."},
                {"type": "text", "text": "world"}
            ]
        });
        assert_eq!(extract_text_content(&val), "hello\nworld");
    }

    #[test]
    fn test_extract_text_content_empty() {
        let val = serde_json::json!({"other": "field"});
        assert_eq!(extract_text_content(&val), "");
    }

    #[test]
    fn test_sync_state_roundtrip() {
        let mut state = HashMap::new();
        state.insert("/path/to/file.md".to_string(), 1234567890u64);
        let json = serde_json::to_string(&state).unwrap();
        let loaded: HashMap<String, u64> = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.get("/path/to/file.md"), Some(&1234567890));
    }
}
