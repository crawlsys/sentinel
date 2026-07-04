//! Session Index Hook — index session transcript to Qdrant on `PreCompact`
//!
//! Fires on `PreCompact`. Reads the session transcript JSONL, chunks it into
//! user+assistant exchanges, and upserts each chunk to the Qdrant
//! `claude-sessions` collection. This makes full conversation history
//! semantically searchable across sessions.
//!
//! Uses `VectorStorePort` + `FileSystemPort` — hooks must not call MCP
//! tools or touch `std::fs`/`reqwest` directly.

use sentinel_domain::constants;
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::VectorPoint;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use super::{FileSystemPort, HookContext};

/// Collection name for session data (NOT claude-memory).
const COLLECTION: &str = "claude-sessions";

/// Minimum combined content length for a chunk to be worth indexing.
const MIN_CHUNK_CHARS: usize = constants::MIN_CHUNK_CHARS;

// ---------------------------------------------------------------------------
// Project hashing (same as memory_inject.rs / task_persist.rs)
// ---------------------------------------------------------------------------

fn project_hash(cwd: &str) -> String {
    super::project_hash(cwd)
}

/// Derive project name from cwd (last path component)
fn project_name(cwd: &str) -> String {
    std::path::Path::new(cwd).file_name().map_or_else(
        || "unknown".to_string(),
        |n| n.to_string_lossy().to_string(),
    )
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
            "yes",
            "no",
            "ok",
            "okay",
            "done",
            "thanks",
            "thank you",
            "got it",
            "sure",
            "y",
            "n",
            "yep",
            "nope",
            "continue",
            "go",
            "next",
            "fix it",
            "all",
            "yee",
            "cool",
            "nice",
            "great",
            "perfect",
            "keep going",
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

/// Extract tool names from `tool_use` blocks in content
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

/// Extract file paths from `tool_use` inputs (Read, Write, Edit tools)
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

/// Parse a transcript JSONL file into exchanges using the injected filesystem port.
fn parse_transcript(fs: &dyn FileSystemPort, path: &str) -> Vec<Exchange> {
    let content = match fs.read_to_string(Path::new(path)) {
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
// Build VectorPoint batch from exchanges
// ---------------------------------------------------------------------------

fn build_points(
    exchanges: &[Exchange],
    session_id: &str,
    project: &str,
    proj_hash: &str,
) -> Vec<VectorPoint> {
    let now = chrono::Utc::now().to_rfc3339();

    exchanges
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

            VectorPoint {
                id,
                text: embed_text,
                payload: serde_json::json!({
                    "session_id": session_id,
                    "project": project,
                    "project_hash": proj_hash,
                    "timestamp": now,
                    "chunk_type": "exchange",
                    "chunk_index": i,
                    "tool_names": ex.tool_names,
                    "files_touched": ex.files_touched,
                    "content": content,
                }),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hook entry point
// ---------------------------------------------------------------------------

/// Process `PreCompact` — read transcript, chunk into exchanges, upsert to Qdrant.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
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

    // Verify transcript file exists (via FileSystemPort)
    if !ctx.fs.exists(&PathBuf::from(transcript_path)) {
        debug!(path = transcript_path, "Transcript file not found");
        return HookOutput::allow();
    }

    // Require vector store to be configured
    let vector_store = if let Some(vs) = ctx.vector_store {
        vs
    } else {
        debug!("No Qdrant vector store configured — skipping session index");
        return HookOutput::allow();
    };

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project = project_name(cwd);
    let proj_hash = project_hash(cwd);

    // Parse transcript into exchanges
    let exchanges = parse_transcript(ctx.fs, transcript_path);
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

    let points = build_points(&exchanges, session_id, &project, &proj_hash);
    if points.is_empty() {
        debug!("No substantive exchanges to upsert");
        return HookOutput::allow();
    }

    let upserted = points.len();
    let ok = super::run_async(async {
        match vector_store.upsert_points(COLLECTION, points).await {
            Ok(()) => true,
            Err(e) => {
                warn!(error = %e, "Session index upsert failed");
                false
            }
        }
    });

    if ok {
        info!(
            upserted,
            total = total_exchanges,
            session = session_id,
            project = project,
            "Session index complete"
        );
    }

    // Never block — this is observational
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/operator/projects/firefly");
        let h2 = project_hash("/Users/operator/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_project_name_extraction() {
        assert_eq!(project_name("/Users/operator/projects/firefly"), "firefly");
        #[cfg(windows)]
        assert_eq!(
            project_name("C:\\Users\\operator\\Documents\\GitHub\\sentinel"),
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
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_no_transcript() {
        let input = HookInput {
            session_id: Some("test-session".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
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
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_parse_transcript_empty() {
        let fs = crate::hooks::test_support::StubFs;
        let exchanges = parse_transcript(&fs, "/nonexistent/path");
        assert!(exchanges.is_empty());
    }

    /// In-memory FileSystemPort that returns a preloaded string from read_to_string.
    #[cfg(test)]
    struct InMemoryFs(String);

    #[cfg(test)]
    impl FileSystemPort for InMemoryFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/mock/home"))
        }
        fn read_to_string(
            &self,
            _path: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(self.0.clone())
        }
        fn write(
            &self,
            _path: &Path,
            _content: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
        fn create_dir_all(
            &self,
            _path: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
        fn read_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, _path: &Path) -> bool {
            true
        }
        fn is_dir(&self, _path: &Path) -> bool {
            false
        }
        fn metadata(
            &self,
            _path: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "no metadata in stub",
            ))
        }
        fn append(
            &self,
            _path: &Path,
            _content: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

    #[test]
    fn test_parse_transcript_valid() {
        let content = r#"{"type":"human","message":{"content":"Hello"}}
{"type":"assistant","message":{"content":"Hi there! How can I help?"}}
{"type":"human","message":{"content":"Explain ownership"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Ownership is a key concept in Rust."},{"type":"tool_use","name":"Read","input":{"file_path":"/src/main.rs"}}]}}
"#
        .to_string();
        let fs = InMemoryFs(content);

        let exchanges = parse_transcript(&fs, "/any/path.jsonl");
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
