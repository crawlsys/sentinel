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

use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::debug;

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

/// Process Stop — sync recently modified memory files to Qdrant.
pub fn process(input: &HookInput) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

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
}
