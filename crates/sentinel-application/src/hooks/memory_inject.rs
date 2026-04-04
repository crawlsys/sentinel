//! Memory Inject Hook — search Qdrant on every prompt and inject relevant memories
//!
//! Fires on UserPromptSubmit. Takes the user's prompt, queries Qdrant Cloud
//! for semantically similar memories, and injects the top results into context.
//!
//! Uses raw reqwest (not MCP tools — hooks can't call MCP tools).
//! Must be fast (<500ms) — uses aggressive timeout.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tracing::{debug, warn};

/// Qdrant config (mirrors qdrant-adapters/config.rs)
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

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qdrant").join("config.json"))
}

fn load_config() -> Option<QdrantConfig> {
    let path = config_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Compute project hash from cwd (same as task_persist/todo_interceptor)
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Search Qdrant synchronously (blocking in a tokio runtime).
/// Returns formatted memory results for context injection.
fn search_qdrant(config: &QdrantConfig, query: &str, project_hash: &str) -> Option<String> {
    // Build the search request
    let body = serde_json::json!({
        "query": {
            "text": query,
            "model": config.model
        },
        "using": "text-dense",
        "limit": 5,
        "with_payload": true,
        "params": {
            "hnsw_ef": 64
        }
    });

    let url = format!(
        "{}/collections/{}/points/query",
        config.cluster_url, config.collection
    );

    // Use a short-lived tokio runtime for the async HTTP call
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;

    let result = rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(800))
            .build()
            .ok()?;

        let resp = client
            .post(&url)
            .header("api-key", &config.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .ok()?;

        let json: serde_json::Value = resp.json().await.ok()?;
        let points = json
            .get("result")?
            .get("points")?
            .as_array()?;

        if points.is_empty() {
            return None;
        }

        // Filter: only include results with score > 0.25 (meaningful similarity)
        let relevant: Vec<&serde_json::Value> = points
            .iter()
            .filter(|p| {
                p.get("score")
                    .and_then(|s| s.as_f64())
                    .map_or(false, |s| s > 0.25)
            })
            .collect();

        if relevant.is_empty() {
            return None;
        }

        let mut output = format!("[Qdrant Memory] {} relevant memor(ies) for this context:\n", relevant.len());

        for point in &relevant {
            let score = point.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
            let payload = point.get("payload")?;
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed");
            let mem_type = payload.get("memory_type").and_then(|v| v.as_str()).unwrap_or("unknown");
            let project = payload.get("project").and_then(|v| v.as_str()).unwrap_or("");
            let content = payload.get("content").and_then(|v| v.as_str()).unwrap_or("");

            // Truncate content for context injection (save tokens)
            let truncated = if content.len() > 300 {
                format!("{}...", &content[..297])
            } else {
                content.to_string()
            };

            output.push_str(&format!(
                "\n- [{:.2}] **{}** ({}, {}):\n  {}\n",
                score, name, mem_type, project, truncated
            ));
        }

        Some(output)
    });

    result
}

/// Process UserPromptSubmit — search Qdrant and inject relevant memories.
pub fn process(input: &HookInput) -> HookOutput {
    // Skip if no prompt or prompt is too short
    let prompt = match input.prompt.as_deref() {
        Some(p) if p.len() > 10 => p,
        _ => return HookOutput::allow(),
    };

    // Skip if prompt looks like a slash command (skill router handles those)
    if prompt.trim().starts_with('/') {
        return HookOutput::allow();
    }

    // Load Qdrant config
    let config = match load_config() {
        Some(c) => c,
        None => {
            debug!("No Qdrant config found — skipping memory injection");
            return HookOutput::allow();
        }
    };

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    // Search Qdrant
    match search_qdrant(&config, prompt, &proj_hash) {
        Some(context) => {
            debug!(memories = context.lines().count(), "Injecting Qdrant memories");
            HookOutput::inject_context(HookEvent::UserPromptSubmit, &context)
        }
        None => {
            debug!("No relevant memories found");
            HookOutput::allow()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_hash() {
        let h = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h.len(), 8);
    }

    #[test]
    fn test_process_no_config() {
        let input = HookInput {
            prompt: Some("test prompt with enough length".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        // Should allow without config (no Qdrant setup)
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_short_prompt() {
        let input = HookInput {
            prompt: Some("hi".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_slash_command() {
        let input = HookInput {
            prompt: Some("/commit".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.hook_specific_output.is_none());
    }
}
