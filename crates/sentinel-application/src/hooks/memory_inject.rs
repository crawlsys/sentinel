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

/// A merged search result from either collection.
struct SearchHit {
    score: f64,
    name: String,
    source: String, // "memory" or "session"
    project: String,
    content: String,
}

/// Search a single Qdrant collection and return hits.
async fn search_collection(
    client: &reqwest::Client,
    config: &QdrantConfig,
    collection: &str,
    query: &str,
    limit: u32,
    min_score: f64,
) -> Vec<SearchHit> {
    let body = serde_json::json!({
        "query": { "text": query, "model": config.model },
        "using": "text-dense",
        "limit": limit,
        "with_payload": true,
        "params": { "hnsw_ef": 64 }
    });

    let url = format!("{}/collections/{}/points/query", config.cluster_url, collection);

    let resp = match client
        .post(&url)
        .header("api-key", &config.api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return vec![],
    };

    let points = json
        .get("result")
        .and_then(|r| r.get("points"))
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let source = if collection == "claude-sessions" { "session" } else { "memory" };

    points
        .iter()
        .filter_map(|p| {
            let score = p.get("score")?.as_f64()?;
            if score < min_score {
                return None;
            }
            let payload = p.get("payload")?;
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
            let project = payload.get("project").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let content = payload.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some(SearchHit { score, name, source: source.to_string(), project, content })
        })
        .collect()
}

/// Search both Qdrant collections and return merged formatted results.
fn search_qdrant(config: &QdrantConfig, query: &str, _project_hash: &str) -> Option<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;

    let result = rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(800))
            .build()
            .ok()?;

        // Search both collections in parallel
        let (memories, sessions) = tokio::join!(
            search_collection(&client, config, &config.collection, query, 3, 0.30),
            search_collection(&client, config, "claude-sessions", query, 3, 0.35),
        );

        // Merge and sort by score
        let mut all: Vec<SearchHit> = memories.into_iter().chain(sessions).collect();
        all.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        // Cap at 5 total
        all.truncate(5);

        if all.is_empty() {
            return None;
        }

        let mem_count = all.iter().filter(|h| h.source == "memory").count();
        let ses_count = all.iter().filter(|h| h.source == "session").count();
        let mut output = format!(
            "[Qdrant Memory] {} relevant hit(s) ({} memories, {} sessions):\n",
            all.len(), mem_count, ses_count
        );

        for hit in &all {
            let truncated = if hit.content.len() > 300 {
                format!("{}...", &hit.content[..297])
            } else {
                hit.content.clone()
            };

            let icon = if hit.source == "session" { "Session" } else { "Memory" };
            output.push_str(&format!(
                "\n- [{:.2}] [{}] **{}** ({}):\n  {}\n",
                hit.score, icon, hit.name, hit.project, truncated
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
