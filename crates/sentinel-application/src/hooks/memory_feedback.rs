//! Memory Feedback Hook — track which injected memories were useful vs wrong
//!
//! Fires on Stop. Reads the state file written by `memory_inject` to find
//! which memories were injected this turn, then checks the assistant response
//! and the user's recent prompt for signals:
//!
//! - **Usage signal**: Memory name appears in assistant response -> boost `access_count`
//! - **Correction signal**: User prompt contains "no", "that's wrong", "actually",
//!   etc. after memories were injected -> flag for manual review
//!
//! Boost: increments `access_count` and updates `accessed_at` via Qdrant `set_payload`.
//! Corrections: appended to `~/.claude/sentinel/state/memory-corrections.jsonl`.

use chrono::Utc;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;
use tracing::debug;

// ---------------------------------------------------------------------------
// Qdrant config (same pattern as memory_inject / memory_extract)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct QdrantConfig {
    cluster_url: String,
    api_key: String,
    #[serde(default = "default_collection")]
    collection: String,
}

fn default_collection() -> String {
    "claude-memory".to_string()
}

fn load_config() -> Option<QdrantConfig> {
    let path = dirs::home_dir()?.join(".qdrant").join("config.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

// ---------------------------------------------------------------------------
// State file types
// ---------------------------------------------------------------------------

/// One injected memory entry from the state file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct InjectedMemory {
    id: String,
    name: String,
    score: f64,
}

/// The full state file written by `memory_inject` on each `UserPromptSubmit`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct InjectedState {
    memories: Vec<InjectedMemory>,
    timestamp: String,
    #[serde(default)]
    user_prompt: Option<String>,
}

/// A single correction log entry written to the JSONL file.
#[derive(Debug, serde::Serialize)]
struct CorrectionEntry {
    timestamp: String,
    memory_id: String,
    memory_name: String,
    correction_signal: String,
    user_prompt: String,
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("sentinel").join("state"))
}

fn injected_state_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("last-injected-memories.json"))
}

fn corrections_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("memory-corrections.jsonl"))
}

// ---------------------------------------------------------------------------
// Correction detection
// ---------------------------------------------------------------------------

/// Phrases that signal the user is correcting a previous assistant response.
const CORRECTION_SIGNALS: &[&str] = &[
    "no,",
    "no.",
    "that's wrong",
    "thats wrong",
    "not anymore",
    "that's outdated",
    "thats outdated",
    "actually,",
    "actually ",
    "incorrect",
    "that's not right",
    "thats not right",
    "wrong",
    "that was wrong",
    "not true",
    "that's old",
    "thats old",
    "out of date",
    "no longer",
    "not correct",
];

/// Check if the user prompt contains any correction signal.
/// Returns the first matching signal, or None.
fn detect_correction(prompt: &str) -> Option<&'static str> {
    let lower = prompt.to_ascii_lowercase();
    CORRECTION_SIGNALS
        .iter()
        .find(|&&signal| lower.contains(signal))
        .copied()
}

// ---------------------------------------------------------------------------
// Memory usage detection
// ---------------------------------------------------------------------------

/// Check if any injected memory name appears in the assistant response text.
fn detect_used_memories<'a>(
    memories: &'a [InjectedMemory],
    response: &str,
) -> Vec<&'a InjectedMemory> {
    let lower_response = response.to_ascii_lowercase();
    memories
        .iter()
        .filter(|m| {
            // Only match if the name is non-trivial (>3 chars) to avoid false positives
            let name_lower = m.name.to_ascii_lowercase();
            name_lower.len() > 3 && lower_response.contains(&name_lower)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Qdrant boost — increment access_count, update accessed_at
// ---------------------------------------------------------------------------

fn boost_memory(config: &QdrantConfig, memory_id: &str) -> bool {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return false,
    };

    rt.block_on(async {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
        {
            Ok(c) => c,
            Err(_) => return false,
        };

        // Step 1: Get current access_count via scroll (single point)
        let scroll_url = format!(
            "{}/collections/{}/points/{}",
            config.cluster_url, config.collection, memory_id
        );

        let current_count: u64 = match client
            .get(&scroll_url)
            .header("api-key", &config.api_key)
            .send()
            .await
        {
            Ok(resp) => {
                let json: serde_json::Value = match resp.json().await {
                    Ok(j) => j,
                    Err(_) => return false,
                };
                json.get("result")
                    .and_then(|r| r.get("payload"))
                    .and_then(|p| p.get("access_count"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            }
            Err(_) => 0,
        };

        // Step 2: set_payload with incremented count
        let payload_url = format!(
            "{}/collections/{}/points/payload",
            config.cluster_url, config.collection
        );

        let body = serde_json::json!({
            "points": [memory_id],
            "payload": {
                "access_count": current_count + 1,
                "accessed_at": Utc::now().to_rfc3339()
            }
        });

        client
            .post(&payload_url)
            .header("api-key", &config.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .is_ok()
    })
}

// ---------------------------------------------------------------------------
// Correction logging
// ---------------------------------------------------------------------------

fn log_correction(memory: &InjectedMemory, signal: &str, user_prompt: &str) {
    let path = match corrections_path() {
        Some(p) => p,
        None => return,
    };

    // Ensure parent dir exists
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let entry = CorrectionEntry {
        timestamp: Utc::now().to_rfc3339(),
        memory_id: memory.id.clone(),
        memory_name: memory.name.clone(),
        correction_signal: signal.to_string(),
        user_prompt: user_prompt.chars().take(500).collect(),
    };

    if let Ok(line) = serde_json::to_string(&entry) {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Process Stop — check if injected memories were used or corrected.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // 1. Read the state file
    let state_path = match injected_state_path() {
        Some(p) if p.exists() => p,
        _ => {
            debug!("No injected-memories state file — skipping feedback");
            return HookOutput::allow();
        }
    };

    let state_content = match std::fs::read_to_string(&state_path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: InjectedState = match serde_json::from_str(&state_content) {
        Ok(s) => s,
        Err(_) => {
            debug!("Invalid injected-memories state file — skipping");
            return HookOutput::allow();
        }
    };

    if state.memories.is_empty() {
        return HookOutput::allow();
    }

    // 2. Read the last assistant message
    let response = match input.last_assistant_message.as_deref() {
        Some(r) if !r.is_empty() => r,
        _ => {
            debug!("No assistant message available — skipping feedback");
            return HookOutput::allow();
        }
    };

    // 3. Detect which memories were used
    let used = detect_used_memories(&state.memories, response);

    // 4. Detect correction patterns (from user prompt stored in state file)
    let correction = state
        .user_prompt
        .as_deref()
        .and_then(detect_correction);

    // Early exit if nothing to do
    if used.is_empty() && correction.is_none() {
        debug!(
            injected = state.memories.len(),
            "No usage or correction detected — skipping feedback"
        );
        return HookOutput::allow();
    }

    // 5. Load Qdrant config for boosting
    let config = load_config();

    // 6. Boost used memories
    let mut boosted = 0;
    if let Some(ref cfg) = config {
        for memory in &used {
            if boost_memory(cfg, &memory.id) {
                boosted += 1;
                debug!(id = %memory.id, name = %memory.name, "Boosted memory access_count");
            }
        }
    }

    // 7. Log corrections
    if let Some(signal) = correction {
        let user_prompt = state.user_prompt.as_deref().unwrap_or("");
        for memory in &state.memories {
            log_correction(memory, signal, user_prompt);
            debug!(
                id = %memory.id,
                name = %memory.name,
                signal,
                "Flagged memory for correction review"
            );
        }
    }

    if boosted > 0 || correction.is_some() {
        debug!(
            boosted,
            corrected = correction.is_some(),
            "Memory feedback complete"
        );
    }

    // Never block
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_correction_positive() {
        assert!(detect_correction("No, that's not what I meant").is_some());
        assert!(detect_correction("That's wrong, the API uses POST").is_some());
        assert!(detect_correction("actually, it should be v2").is_some());
        assert!(detect_correction("that's outdated info").is_some());
        assert!(detect_correction("Not anymore, we migrated").is_some());
        assert!(detect_correction("incorrect — we use Rust now").is_some());
        assert!(detect_correction("That's not right").is_some());
        assert!(detect_correction("no longer relevant").is_some());
    }

    #[test]
    fn test_detect_correction_negative() {
        assert!(detect_correction("Tell me about the deploy process").is_none());
        assert!(detect_correction("How do I fix this error?").is_none());
        assert!(detect_correction("What's the architecture?").is_none());
        assert!(detect_correction("List all the hooks").is_none());
        // "know" contains "no" but we match "no," and "no." specifically
        assert!(detect_correction("I know the answer").is_none());
    }

    #[test]
    fn test_detect_used_memories_found() {
        let memories = vec![
            InjectedMemory {
                id: "id1".to_string(),
                name: "Firefly Pro CRM".to_string(),
                score: 0.85,
            },
            InjectedMemory {
                id: "id2".to_string(),
                name: "Sentinel Hook Engine".to_string(),
                score: 0.75,
            },
        ];

        let response = "The Firefly Pro CRM uses Next.js 15 with Material UI.";
        let used = detect_used_memories(&memories, response);
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].name, "Firefly Pro CRM");
    }

    #[test]
    fn test_detect_used_memories_none() {
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            name: "Firefly Pro CRM".to_string(),
            score: 0.85,
        }];

        let response = "The deployment process uses Railway for hosting.";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty());
    }

    #[test]
    fn test_detect_used_memories_short_name_skipped() {
        // Names <= 3 chars should be skipped to avoid false positives
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            name: "api".to_string(),
            score: 0.80,
        }];

        let response = "The API endpoint returns JSON";
        let used = detect_used_memories(&memories, response);
        assert!(used.is_empty());
    }

    #[test]
    fn test_detect_used_memories_case_insensitive() {
        let memories = vec![InjectedMemory {
            id: "id1".to_string(),
            name: "Qdrant Vector Database".to_string(),
            score: 0.90,
        }];

        let response = "We use qdrant vector database for semantic search.";
        let used = detect_used_memories(&memories, response);
        assert_eq!(used.len(), 1);
    }

    #[test]
    fn test_process_no_state_file() {
        let input = HookInput {
            last_assistant_message: Some("response text".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_process_no_assistant_message() {
        let input = HookInput {
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_correction_entry_serializes() {
        let entry = CorrectionEntry {
            timestamp: "2026-04-04T12:00:00Z".to_string(),
            memory_id: "test-id".to_string(),
            memory_name: "Test Memory".to_string(),
            correction_signal: "actually,".to_string(),
            user_prompt: "actually, that's different now".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("test-id"));
        assert!(json.contains("actually,"));
    }

    #[test]
    fn test_injected_state_deserializes() {
        let json = r#"{
            "memories": [
                {"id": "abc", "name": "Test", "score": 0.85}
            ],
            "timestamp": "2026-04-04T12:00:00Z",
            "user_prompt": "test prompt"
        }"#;
        let state: InjectedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.memories.len(), 1);
        assert_eq!(state.memories[0].id, "abc");
        assert_eq!(state.user_prompt.as_deref(), Some("test prompt"));
    }

    #[test]
    fn test_injected_state_no_prompt() {
        let json = r#"{
            "memories": [],
            "timestamp": "2026-04-04T12:00:00Z"
        }"#;
        let state: InjectedState = serde_json::from_str(json).unwrap();
        assert!(state.user_prompt.is_none());
    }
}
