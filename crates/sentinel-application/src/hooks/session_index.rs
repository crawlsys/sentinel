//! Session Index Hook — index session transcript to Qdrant on PreCompact
//!
//! Fires on PreCompact. Reads the session transcript JSONL, chunks it into
//! user+assistant exchanges, and upserts each chunk to Qdrant Cloud's
//! `claude-sessions` collection. This makes full conversation history
//! semantically searchable across sessions.
//!
//! Uses raw reqwest (not MCP tools — hooks can't call MCP tools).
//! Same pattern as memory_inject.rs / memory_extract.rs.

use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Qdrant config (mirrors memory_inject.rs)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct QdrantConfig {
    cluster_url: String,
    api_key: String,
    #[serde(default = "default_model")]
    model: String,
}

fn default_model() -> String {
    "sentence-transformers/all-MiniLM-L6-v2".to_string()
}

fn load_config() -> Option<QdrantConfig> {
    let path = dirs::home_dir()?.join(".qdrant").join("config.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Collection name for session data (NOT claude-memory)
const COLLECTION: &str = "claude-sessions";

/// Minimum combined content length for a chunk to be worth indexing
const MIN_CHUNK_CHARS: usize = 50;

// ---------------------------------------------------------------------------
// Project hashing (same as memory_inject.rs / task_persist.rs)
// ---------------------------------------------------------------------------

fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Derive project name from cwd (last path component)
fn project_name(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// Deterministic UUID from content hash (same pattern as memory_extract.rs)
// ---------------------------------------------------------------------------

fn content_to_uuid(session_id: &str, chunk_index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{session_id}:chunk:{chunk_index}").as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&result[..16]);
    // UUID v4 variant bits
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

// ---------------------------------------------------------------------------
// Transcript parsing — JSONL to Exchange chunks
// ---------------------------------------------------------------------------

/// A single exchange: user prompt + assistant response
#[derive(Debug)]
struct Exchange {
    user_text: String,
    assistant_text: String,
    tool_names: Vec<String>,
    files_touched: Vec<String>,
}

impl Exchange {
    fn combined_content(&self) -> String {
        let mut content = String::new();
        if !self.user_text.is_empty() {
            content.push_str("User: ");
            content.push_str(&self.user_text);
            content.push('\n');
        }
        if !self.assistant_text.is_empty() {
            content.push_str("Assistant: ");
            content.push_str(&self.assistant_text);
        }
        content
    }

    fn is_substantive(&self) -> bool {
        let combined_len = self.combined_content().len();
        if combined_len < MIN_CHUNK_CHARS {
            return false;
        }

        // Skip exchanges where user input is trivial acknowledgement
        let user_trimmed = self.user_text.trim().to_lowercase();
        let trivial = [
            "yes", "no", "ok", "okay", "done", "thanks", "thank you", "got it",
            "sure", "y", "n", "yep", "nope", "continue", "go", "next", "fix it",
            "all", "yee", "cool", "nice", "great", "perfect", "keep going",
        ];
        if trivial.contains(&user_trimmed.as_str()) && self.assistant_text.len() < 200 {
            return false;
        }

        true
    }
}

/// Extract text content from a message content array or string
fn extract_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut text_parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                    if t == "text" {
                        if let Some(txt) = item.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(txt.to_string());
                        }
                    }
                }
            }
            text_parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Extract tool names from tool_use blocks in content
fn extract_tool_names(content: &serde_json::Value) -> Vec<String> {
    let mut tools = Vec::new();
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                if t == "tool_use" {
                    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                        if !tools.contains(&name.to_string()) {
                            tools.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    tools
}

/// Extract file paths from tool_use inputs (Read, Write, Edit tools)
fn extract_files(content: &serde_json::Value) -> Vec<String> {
    let mut files = Vec::new();
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                if t == "tool_use" {
                    if let Some(input) = item.get("input") {
                        // Check common file path fields
                        for key in &["file_path", "path", "command"] {
                            if let Some(val) = input.get(*key).and_then(|v| v.as_str()) {
                                // Only capture things that look like file paths
                                if val.contains('/') || val.contains('\\') {
                                    let file = val.to_string();
                                    if !files.contains(&file) {
                                        files.push(file);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    files
}

/// Parse a transcript JSONL file into exchanges
fn parse_transcript(path: &str) -> Vec<Exchange> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path, error = %e, "Failed to read transcript");
            return Vec::new();
        }
    };

    let mut exchanges = Vec::new();
    let mut current_user = String::new();
    let mut current_assistant = String::new();
    let mut current_tools: Vec<String> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
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
            .or_else(|| val.get("content"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        match msg_type {
            "human" | "user" => {
                // Flush previous exchange if we had one
                if in_exchange && (!current_user.is_empty() || !current_assistant.is_empty()) {
                    exchanges.push(Exchange {
                        user_text: std::mem::take(&mut current_user),
                        assistant_text: std::mem::take(&mut current_assistant),
                        tool_names: std::mem::take(&mut current_tools),
                        files_touched: std::mem::take(&mut current_files),
                    });
                }
                current_user = extract_text(&msg_content);
                in_exchange = true;
            }
            "assistant" => {
                let text = extract_text(&msg_content);
                if !text.is_empty() {
                    if !current_assistant.is_empty() {
                        current_assistant.push('\n');
                    }
                    current_assistant.push_str(&text);
                }
                // Extract tool names and files from assistant content blocks
                let tools = extract_tool_names(&msg_content);
                for t in tools {
                    if !current_tools.contains(&t) {
                        current_tools.push(t);
                    }
                }
                let files = extract_files(&msg_content);
                for f in files {
                    if !current_files.contains(&f) {
                        current_files.push(f);
                    }
                }
                in_exchange = true;
            }
            // Skip system messages, tool_result, etc.
            _ => {}
        }
    }

    // Flush final exchange
    if in_exchange && (!current_user.is_empty() || !current_assistant.is_empty()) {
        exchanges.push(Exchange {
            user_text: current_user,
            assistant_text: current_assistant,
            tool_names: current_tools,
            files_touched: current_files,
        });
    }

    exchanges
}

// ---------------------------------------------------------------------------
// Qdrant upsert — batch upsert exchanges
// ---------------------------------------------------------------------------

fn upsert_exchanges(
    config: &QdrantConfig,
    exchanges: &[Exchange],
    session_id: &str,
    project: &str,
    proj_hash: &str,
) -> usize {
    if exchanges.is_empty() {
        return 0;
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            warn!(error = %e, "Failed to build tokio runtime");
            return 0;
        }
    };

    rt.block_on(async {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return 0,
        };

        let url = format!(
            "{}/collections/{}/points?wait=true",
            config.cluster_url, COLLECTION
        );

        let now = chrono::Utc::now().to_rfc3339();

        // Build points array — one per exchange
        let points: Vec<serde_json::Value> = exchanges
            .iter()
            .enumerate()
            .filter(|(_, ex)| ex.is_substantive())
            .map(|(i, ex)| {
                let id = content_to_uuid(session_id, i);
                let content = ex.combined_content();

                // Truncate content for embedding (model has token limits)
                let embed_text = if content.len() > 2000 {
                    format!("{}...", &content[..1997])
                } else {
                    content.clone()
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
                        "chunk_type": "exchange",
                        "chunk_index": i,
                        "tool_names": ex.tool_names,
                        "files_touched": ex.files_touched,
                        "content": content
                    }
                })
            })
            .collect();

        if points.is_empty() {
            return 0;
        }

        let total = points.len();

        // Batch in groups of 20 to avoid oversized requests
        let mut upserted = 0;
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
                    upserted += batch.len();
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body_text = resp.text().await.unwrap_or_default();
                    warn!(
                        status = %status,
                        body = %body_text,
                        "Qdrant upsert returned non-success"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Qdrant upsert request failed");
                }
            }
        }

        if upserted > 0 {
            info!(
                upserted,
                total,
                session = session_id,
                "Session chunks indexed to Qdrant"
            );
        }

        upserted
    })
}

// ---------------------------------------------------------------------------
// Hook entry point
// ---------------------------------------------------------------------------

/// Process PreCompact — read transcript, chunk into exchanges, upsert to Qdrant.
pub fn process(input: &HookInput) -> HookOutput {
    let session_id = match input.session_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => {
            debug!("No session_id — skipping session index");
            return HookOutput::allow();
        }
    };

    let transcript_path = match input.transcript_path.as_deref() {
        Some(p) if !p.is_empty() => p,
        _ => {
            debug!("No transcript_path — skipping session index");
            return HookOutput::allow();
        }
    };

    // Verify transcript file exists
    if !PathBuf::from(transcript_path).exists() {
        debug!(path = transcript_path, "Transcript file not found");
        return HookOutput::allow();
    }

    // Load Qdrant config
    let config = match load_config() {
        Some(c) => c,
        None => {
            debug!("No Qdrant config found — skipping session index");
            return HookOutput::allow();
        }
    };

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = project_name(cwd);
    let proj_hash = project_hash(cwd);

    // Parse transcript into exchanges
    let exchanges = parse_transcript(transcript_path);
    if exchanges.is_empty() {
        debug!("No exchanges found in transcript");
        return HookOutput::allow();
    }

    let total_exchanges = exchanges.len();
    debug!(
        total = total_exchanges,
        session = session_id,
        "Parsed exchanges from transcript"
    );

    // Upsert to Qdrant
    let upserted = upsert_exchanges(&config, &exchanges, session_id, &project, &proj_hash);

    info!(
        upserted,
        total = total_exchanges,
        session = session_id,
        project = project,
        "Session index complete"
    );

    // Never block — this is observational
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_project_name_extraction() {
        assert_eq!(project_name("/Users/gary/projects/firefly"), "firefly");
        assert_eq!(
            project_name("C:\\Users\\gary\\Documents\\GitHub\\sentinel"),
            "sentinel"
        );
    }

    #[test]
    fn test_content_to_uuid_deterministic() {
        let id1 = content_to_uuid("session-abc", 0);
        let id2 = content_to_uuid("session-abc", 0);
        assert_eq!(id1, id2);

        // Different chunk index => different UUID
        let id3 = content_to_uuid("session-abc", 1);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_extract_text_string() {
        let val = serde_json::json!("hello world");
        assert_eq!(extract_text(&val), "hello world");
    }

    #[test]
    fn test_extract_text_array() {
        let val = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "name": "Read"},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(extract_text(&val), "hello\nworld");
    }

    #[test]
    fn test_extract_tool_names() {
        let val = serde_json::json!([
            {"type": "text", "text": "some text"},
            {"type": "tool_use", "name": "Read", "input": {}},
            {"type": "tool_use", "name": "Edit", "input": {}},
            {"type": "tool_use", "name": "Read", "input": {}} // duplicate
        ]);
        let tools = extract_tool_names(&val);
        assert_eq!(tools, vec!["Read", "Edit"]);
    }

    #[test]
    fn test_extract_files() {
        let val = serde_json::json!([
            {"type": "tool_use", "name": "Read", "input": {"file_path": "/src/main.rs"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "/src/lib.rs"}}
        ]);
        let files = extract_files(&val);
        assert_eq!(files, vec!["/src/main.rs", "/src/lib.rs"]);
    }

    #[test]
    fn test_exchange_combined_content() {
        let ex = Exchange {
            user_text: "What is Rust?".to_string(),
            assistant_text: "Rust is a systems programming language.".to_string(),
            tool_names: vec![],
            files_touched: vec![],
        };
        let content = ex.combined_content();
        assert!(content.contains("User: What is Rust?"));
        assert!(content.contains("Assistant: Rust is a systems programming language."));
    }

    #[test]
    fn test_exchange_substantive() {
        let short = Exchange {
            user_text: "hi".to_string(),
            assistant_text: "ok".to_string(),
            tool_names: vec![],
            files_touched: vec![],
        };
        assert!(!short.is_substantive());

        let long = Exchange {
            user_text: "Please explain the borrow checker in Rust".to_string(),
            assistant_text: "The borrow checker enforces ownership rules at compile time"
                .to_string(),
            tool_names: vec![],
            files_touched: vec![],
        };
        assert!(long.is_substantive());
    }

    #[test]
    fn test_process_no_session() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_no_transcript() {
        let input = HookInput {
            session_id: Some("test-session".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_missing_transcript_file() {
        let input = HookInput {
            session_id: Some("test-session".to_string()),
            transcript_path: Some("/nonexistent/path/transcript.jsonl".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_parse_transcript_empty() {
        let exchanges = parse_transcript("/nonexistent/path");
        assert!(exchanges.is_empty());
    }

    #[test]
    fn test_parse_transcript_valid() {
        // Create a temp file with JSONL content
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = r#"{"type":"human","message":{"content":"Hello"}}
{"type":"assistant","message":{"content":"Hi there! How can I help?"}}
{"type":"human","message":{"content":"Explain ownership"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Ownership is a key concept in Rust."},{"type":"tool_use","name":"Read","input":{"file_path":"/src/main.rs"}}]}}
"#;
        std::fs::write(&path, content).unwrap();

        let exchanges = parse_transcript(path.to_str().unwrap());
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user_text, "Hello");
        assert!(exchanges[0].assistant_text.contains("Hi there"));
        assert_eq!(exchanges[1].user_text, "Explain ownership");
        assert!(exchanges[1].tool_names.contains(&"Read".to_string()));
        assert!(exchanges[1]
            .files_touched
            .contains(&"/src/main.rs".to_string()));
    }
}
