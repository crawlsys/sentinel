//! Memory Extract Hook — sync flat-file memories to Qdrant
//!
//! Fires on Stop. Detects memory files that changed since the last sync
//! (tracked via a state file, not a time window) and upserts them to Qdrant.
//!
//! Claude decides what to remember → writes .md file → this hook syncs to Qdrant.
//!
//! **Periodic session re-index:** Every 50 tool calls, indexes the last ~10
//! substantive exchanges to keep long-running sessions searchable.

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
fn sync_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("memory-sync-state.json")
    })
}

fn load_sync_state() -> HashMap<String, u64> {
    let path = match sync_state_path() {
        Some(p) => p,
        None => return HashMap::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_sync_state(state: &HashMap<String, u64>) {
    let path = match sync_state_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(&path, json);
    }
}

/// Get mtime as unix timestamp for a file
fn file_mtime(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Find memory files that have changed since last sync
fn find_unsynced_memories() -> Vec<PathBuf> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.is_dir() {
        return vec![];
    }

    let state = load_sync_state();
    let mut unsynced = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let memory_dir = entry.path().join("memory");
            if !memory_dir.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&memory_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if !name.ends_with(".md") || name == "MEMORY.md" {
                        continue;
                    }
                    let key = path.to_string_lossy().to_string();
                    let current_mtime = file_mtime(&path).unwrap_or(0);
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
// Qdrant config + upsert
// ---------------------------------------------------------------------------

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

/// Compute deterministic UUID from source path
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

/// Upsert a single memory file to Qdrant. Returns true on success.
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

    super::run_async(async {
        let client = match reqwest::Client::builder()
            .timeout(constants::API_CALL_TIMEOUT)
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

const REINDEX_TOOL_CALL_THRESHOLD: u64 = constants::REINDEX_TOOL_CALL_THRESHOLD;
const SESSION_COLLECTION: &str = "claude-sessions";

/// Minimum combined user+assistant text length to index an exchange.
/// Filters out trivial "yes"/"ok"/"done" turns.
const MIN_EXCHANGE_LENGTH: usize = constants::MIN_EXCHANGE_LENGTH;

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

/// Check if an exchange is substantive enough to index.
/// Filters out trivial turns like "yes", "ok", "done", single-word responses.
fn is_substantive_exchange(user: &str, assistant: &str) -> bool {
    let combined_len = user.len() + assistant.len();
    if combined_len < MIN_EXCHANGE_LENGTH {
        return false;
    }

    // Skip exchanges where the user message is trivial
    let user_trimmed = user.trim().to_lowercase();
    let trivial_patterns = [
        "yes", "no", "ok", "okay", "done", "thanks", "thank you", "got it",
        "sure", "y", "n", "yep", "nope", "continue", "go", "next", "fix it",
        "all", "yee", "cool", "nice", "great", "perfect",
    ];
    if trivial_patterns.contains(&user_trimmed.as_str()) && assistant.len() < 200 {
        return false;
    }

    true
}

/// Lightweight session re-index: parse the last ~10 exchanges from the
/// transcript and upsert substantive ones to Qdrant's `claude-sessions` collection.
fn periodic_session_index(
    config: &QdrantConfig,
    transcript_path: &str,
    session_id: &str,
    cwd: &str,
) {
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

    // Build Qdrant points — filter out trivial exchanges
    let points: Vec<serde_json::Value> = recent
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
        debug!("No substantive exchanges to index");
        return;
    }

    // Upsert to Qdrant
    super::run_async(async {
        let client = match reqwest::Client::builder()
            .timeout(constants::API_CALL_TIMEOUT_LONG)
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
                    info!(
                        count = batch.len(),
                        session = session_id,
                        "Periodic session index upserted"
                    );
                }
                Ok(resp) => {
                    let status = resp.status();
                    warn!(
                        status = %status,
                        "Periodic session index upsert returned non-success"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Periodic session index upsert failed");
                }
            }
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
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // --- Periodic session re-index ---
    let mut index_state = load_session_index_state();
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
                && PathBuf::from(transcript_path).exists()
            {
                if let Some(config) = load_config() {
                    periodic_session_index(&config, transcript_path, session_id, cwd);
                }
            }
        }

        index_state.tool_calls_since_index = 0;
        index_state.last_indexed_at = Some(chrono::Utc::now().to_rfc3339());
    }

    save_session_index_state(&index_state);

    // --- Memory file sync (state-tracked, replaces 30s window) ---
    let unsynced = find_unsynced_memories();
    if unsynced.is_empty() {
        return HookOutput::allow();
    }

    debug!(count = unsynced.len(), "Found unsynced memory files");

    let config = match load_config() {
        Some(c) => c,
        None => {
            debug!("No Qdrant config — skipping memory sync");
            return HookOutput::allow();
        }
    };

    // Sync each changed file and update state
    let mut state = load_sync_state();
    let mut synced = 0;
    for path in &unsynced {
        if upsert_memory(&config, path) {
            synced += 1;
            let key = path.to_string_lossy().to_string();
            let mtime = file_mtime(path).unwrap_or(0);
            state.insert(key, mtime);
            debug!(file = %path.display(), "Synced memory to Qdrant");
        }
    }

    if synced > 0 {
        save_sync_state(&state);
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
