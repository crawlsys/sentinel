//! Memory Extract Hook — extract learnings from turns and store in Qdrant
//!
//! Fires on Stop. Analyzes the last assistant message for extractable facts:
//! corrections, decisions, patterns discovered during the turn. If the turn
//! was substantive (had tool calls, file edits, etc.), extracts key learnings
//! and upserts them to Qdrant.
//!
//! IMPORTANT: This hook does NOT use AI classification to decide what to extract.
//! It only extracts when Claude's built-in auto-memory system writes a file
//! (detected by checking for recent writes to the memory directory). This keeps
//! the hook lightweight and avoids double-extraction with Claude's own memory.
//!
//! The primary role is to SYNC flat-file memories to Qdrant, not to independently
//! decide what's worth remembering. Claude decides → writes file → this hook syncs.
//!
//! **Periodic session re-index (Phase 2):** Tracks tool call count via a state
//! file. Every 50 tool calls, triggers a lightweight session index of the last
//! ~10 exchanges to keep long-running sessions searchable without waiting for
//! PreCompact.

use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Check if any memory files were modified in the last 30 seconds
/// (indicating Claude's auto-memory just wrote something).
fn recently_modified_memories(cwd: &str) -> Vec<PathBuf> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    // Scan the project-specific memory dir
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        return vec![];
    }

    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(30))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let mut recent = Vec::new();

    // Check all project memory directories
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let memory_dir = entry.path().join("memory");
            if !memory_dir.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&memory_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if !name.ends_with(".md") || name == "MEMORY.md" {
                        continue;
                    }
                    if let Ok(meta) = std::fs::metadata(&path) {
                        if let Ok(modified) = meta.modified() {
                            if modified > cutoff {
                                recent.push(path);
                            }
                        }
                    }
                }
            }
        }
    }

    recent
}

/// Qdrant config
#[derive(serde::Deserialize)]
struct QdrantConfig {
    cluster_url: String,
    api_key: String,
    #[serde(default = "default_collection")]
    collection: String,
    #[serde(default = "default_model")]
    model: String,
}

fn default_collection() -> String {
    "claude-memory".to_string()
}

fn default_model() -> String {
    "sentence-transformers/all-MiniLM-L6-v2".to_string()
}

fn load_config() -> Option<QdrantConfig> {
    let path = dirs::home_dir()?.join(".qdrant").join("config.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Compute deterministic UUID from source path (same as sync.rs)
fn path_to_uuid(path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&result[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

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

/// Upsert a single memory file to Qdrant
fn upsert_memory(config: &QdrantConfig, path: &PathBuf) -> bool {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let (name, description, mem_type, body) = match parse_frontmatter(&content) {
        Some(fm) => fm,
        None => return false,
    };

    let source_path = path.to_string_lossy().to_string();
    let id = path_to_uuid(&source_path);

    let full_text = if body.is_empty() {
        format!("{name}. {description}")
    } else {
        format!("{name}. {description}\n\n{body}")
    };

    let body_json = serde_json::json!({
        "points": [{
            "id": id,
            "vector": {
                "text-dense": {
                    "text": full_text,
                    "model": config.model
                }
            },
            "payload": {
                "content": full_text,
                "name": name,
                "description": description,
                "memory_type": if mem_type.is_empty() { "project" } else { &mem_type },
                "project": "auto-extract",
                "source_file": source_path,
                "created_at": chrono::Utc::now().to_rfc3339(),
                "access_count": 0
            }
        }]
    });

    let url = format!(
        "{}/collections/{}/points?wait=true",
        config.cluster_url, config.collection
    );

    // Blocking HTTP call
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return false,
    };

    rt.block_on(async {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };

        client
            .put(&url)
            .header("api-key", &config.api_key)
            .header("Content-Type", "application/json")
            .json(&body_json)
            .send()
            .await
            .is_ok()
    })
}

// ---------------------------------------------------------------------------
// Periodic session re-index — every ~50 tool calls
// ---------------------------------------------------------------------------

/// Tool call threshold before triggering a session re-index.
const REINDEX_TOOL_CALL_THRESHOLD: u64 = 50;

/// Session index collection name (same as session_index.rs).
const SESSION_COLLECTION: &str = "claude-sessions";

/// State file for tracking tool calls since last re-index.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SessionIndexState {
    tool_calls_since_index: u64,
    last_indexed_at: Option<String>,
}

fn session_index_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("session-index-state.json")
    })
}

fn load_session_index_state() -> SessionIndexState {
    let path = match session_index_state_path() {
        Some(p) => p,
        None => return SessionIndexState::default(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return SessionIndexState::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_session_index_state(state: &SessionIndexState) {
    let path = match session_index_state_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&path, json);
    }
}

/// Deterministic UUID from session + chunk index (same pattern as session_index.rs).
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

/// Lightweight session re-index: parse the last ~10 exchanges from the
/// transcript and upsert them to Qdrant's `claude-sessions` collection.
fn periodic_session_index(config: &QdrantConfig, transcript_path: &str, session_id: &str, cwd: &str) {
    let content = match std::fs::read_to_string(transcript_path) {
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

        let msg_content = val
            .get("message")
            .and_then(|m| m.get("content"))
            .or_else(|| val.get("content"));

        let text = msg_content
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
                    if parts.is_empty() { None } else { Some(parts.join("\n")) }
                }
                _ => None,
            })
            .unwrap_or_default();

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

    // Build Qdrant points
    let points: Vec<serde_json::Value> = recent
        .iter()
        .enumerate()
        .filter(|(_, (u, a))| u.len() + a.len() >= 50)
        .map(|(i, (user, assistant))| {
            let id = content_to_uuid(session_id, start + i);
            let combined = format!("User: {user}\nAssistant: {assistant}");
            let embed_text = if combined.len() > 2000 {
                format!("{}...", &combined[..1997])
            } else {
                combined.clone()
            };

            serde_json::json!({
                "id": id,
                "vector": {
                    "text-dense": {
                        "text": embed_text,
                        "model": config.model
                    }
                },
                "payload": {
                    "session_id": session_id,
                    "project": project,
                    "project_hash": proj_hash,
                    "timestamp": now,
                    "chunk_type": "periodic_exchange",
                    "chunk_index": start + i,
                    "content": combined
                }
            })
        })
        .collect();

    if points.is_empty() {
        return;
    }

    // Upsert to Qdrant
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };

    rt.block_on(async {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        let url = format!(
            "{}/collections/{}/points?wait=true",
            config.cluster_url, SESSION_COLLECTION
        );

        for batch in points.chunks(20) {
            let body = serde_json::json!({ "points": batch });
            match client
                .put(&url)
                .header("api-key", &config.api_key)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    info!(count = batch.len(), session = session_id, "Periodic session index upserted");
                }
                Ok(resp) => {
                    let status = resp.status();
                    warn!(status = %status, "Periodic session index upsert returned non-success");
                }
                Err(e) => {
                    warn!(error = %e, "Periodic session index upsert failed");
                }
            }
        }
    });
}

/// Process Stop — sync recently modified memory files to Qdrant,
/// and periodically re-index the session transcript.
pub fn process(input: &HookInput) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // --- Periodic session re-index ---
    // Increment tool call counter and check threshold
    let mut index_state = load_session_index_state();
    index_state.tool_calls_since_index += 1;

    if index_state.tool_calls_since_index >= REINDEX_TOOL_CALL_THRESHOLD {
        debug!(
            calls = index_state.tool_calls_since_index,
            "Tool call threshold reached — triggering periodic session index"
        );

        // We need session_id and transcript_path
        if let (Some(session_id), Some(transcript_path)) =
            (input.session_id.as_deref(), input.transcript_path.as_deref())
        {
            if !session_id.is_empty()
                && !transcript_path.is_empty()
                && PathBuf::from(transcript_path).exists()
            {
                if let Some(config) = load_config() {
                    periodic_session_index(&config, transcript_path, session_id, cwd);
                }
            }
        }

        // Reset counter
        index_state.tool_calls_since_index = 0;
        index_state.last_indexed_at = Some(chrono::Utc::now().to_rfc3339());
    }

    save_session_index_state(&index_state);

    // --- Memory file sync (existing behavior) ---

    // Check for recently modified memory files
    let recent = recently_modified_memories(cwd);
    if recent.is_empty() {
        return HookOutput::allow();
    }

    debug!(count = recent.len(), "Found recently modified memory files");

    // Load Qdrant config
    let config = match load_config() {
        Some(c) => c,
        None => {
            debug!("No Qdrant config — skipping memory sync");
            return HookOutput::allow();
        }
    };

    // Sync each modified file
    let mut synced = 0;
    for path in &recent {
        if upsert_memory(&config, path) {
            synced += 1;
            debug!(file = %path.display(), "Synced memory to Qdrant");
        }
    }

    if synced > 0 {
        debug!(synced, "Memory files synced to Qdrant");
    }

    // Never block
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
    fn test_path_to_uuid_deterministic() {
        let id1 = path_to_uuid("test.md");
        let id2 = path_to_uuid("test.md");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_process_no_recent_files() {
        let input = HookInput {
            cwd: Some("/nonexistent".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    // -------------------------------------------------------------------
    // Periodic session re-index tests
    // -------------------------------------------------------------------

    #[test]
    fn test_session_index_state_roundtrip() {
        let state = SessionIndexState {
            tool_calls_since_index: 42,
            last_indexed_at: Some("2026-04-04T10:00:00Z".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: SessionIndexState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tool_calls_since_index, 42);
        assert_eq!(loaded.last_indexed_at.as_deref(), Some("2026-04-04T10:00:00Z"));
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
}
