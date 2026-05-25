//! Memory Session-Index Hook — periodic session-transcript re-indexing.
//!
//! Fires on Stop. Every ~50 tool calls, indexes the last ~10 substantive
//! exchanges into the `claude-sessions` Qdrant collection so long-running
//! sessions stay searchable.
//!
//! NOTE: the legacy flat-`.md` memory-file sync path that used to live here
//! has been removed. Memories are now captured directly from conversation
//! turns by `memory_turn_capture` (LLM extraction → dual-judge
//! `memory_capture`). This hook only owns session-transcript indexing.

use super::{FileSystemPort, VectorPoint, VectorStorePort};
use sentinel_domain::constants;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::{debug, info, warn};

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

/// Compute project hash from cwd. Delegates to the shared canonical
/// implementation in `super::project_hash`.
fn project_hash(cwd: &str) -> String {
    super::project_hash(cwd)
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
            Ok(()) => info!(
                count,
                session = session_id,
                "Periodic session index upserted"
            ),
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

        if let (Some(session_id), Some(transcript_path)) = (
            input.session_id.as_deref(),
            input.transcript_path.as_deref(),
        ) {
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

    // Flat-`.md` memory ingest has been removed. Memories are now captured
    // directly from conversation turns by `memory_turn_capture` (LLM
    // extraction → dual-judge `memory_capture`), so there is no longer a
    // file-scanning sync path here. This hook now only owns periodic session
    // transcript re-indexing (handled above).
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_substantive_exchange() {
        assert!(!is_substantive_exchange("yes", "ok"));
        assert!(!is_substantive_exchange("y", "done"));
        assert!(is_substantive_exchange(
            "fix the authentication bug in login.rs",
            "I found the issue in the token validation. The JWT expiry check was comparing timestamps in different formats."
        ));
        // Short user + long assistant = ok
        assert!(is_substantive_exchange("fix it", &"x".repeat(250)));
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
        #[cfg(windows)]
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

}
