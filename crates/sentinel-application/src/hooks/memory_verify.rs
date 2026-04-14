//! Memory Verify Hook — verify stored memories against ground truth on SessionStart.
//!
//! Runs on SessionStart with a 24h cooldown. Scrolls Qdrant for memories not
//! verified in the last 7 days, extracts claims via Claude API (claude-haiku-4-5-20251001),
//! verifies file_path claims with fs::exists(), and updates Qdrant payloads.
//!
//! All network calls run inside `run_async()` which enforces a 3-second wall-clock
//! timeout — the hook never blocks SessionStart.

use chrono::Utc;
use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use tracing::{debug, warn};

use super::run_async;

/// Maximum memories to verify per session to limit API calls.
const MAX_VERIFY_PER_SESSION: usize = constants::MAX_VERIFY_PER_SESSION;

/// Memories not verified in this many days are eligible for re-verification.
const VERIFY_STALE_DAYS: i64 = constants::VERIFY_STALE_DAYS;

/// 24h cooldown file path.
fn cooldown_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("last-verify.txt")
    })
}

/// Check if 24h cooldown has elapsed.
fn check_cooldown() -> bool {
    let path = match cooldown_path() {
        Some(p) => p,
        None => return true,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return true, // No file = never run
    };
    let ts = match chrono::DateTime::parse_from_rfc3339(content.trim()) {
        Ok(t) => t.with_timezone(&Utc),
        Err(_) => return true,
    };
    let hours_elapsed = (Utc::now() - ts).num_hours();
    hours_elapsed >= 24
}

/// Write cooldown timestamp.
fn write_cooldown() {
    let path = match cooldown_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, Utc::now().to_rfc3339());
}

/// Qdrant config (mirrors qdrant-adapters/config.rs).
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

fn load_qdrant_config() -> Option<QdrantConfig> {
    let path = dirs::home_dir()?.join(".qdrant").join("config.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Load Anthropic API key — tries env var first, then Doppler.
fn load_anthropic_key() -> Option<String> {
    // 1. Check env var (fast path — set by settings.json or parent process)
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }

    // 2. Fall back to Doppler via the service token already in env
    let output = std::process::Command::new("doppler")
        .args(["secrets", "get", "ANTHROPIC_API_KEY", "--plain"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if output.status.success() {
        let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !key.is_empty() {
            return Some(key);
        }
    }

    None
}

/// A memory point from Qdrant scroll.
struct MemoryPoint {
    id: String,
    name: String,
    content: String,
    #[allow(dead_code)]
    last_verified_at: Option<String>,
}

/// A verifiable claim extracted by Cerebras.
#[derive(serde::Deserialize)]
struct Claim {
    claim_type: String,
    #[allow(dead_code)]
    claim_text: String,
    verifiable_value: String,
}

/// Scroll Qdrant for memories not verified in the last N days.
async fn scroll_unverified(
    client: &reqwest::Client,
    config: &QdrantConfig,
) -> Vec<MemoryPoint> {
    let body = serde_json::json!({
        "limit": 100,
        "with_payload": true
    });

    let url = format!(
        "{}/collections/{}/points/scroll",
        config.cluster_url, config.collection
    );

    let resp = match client
        .post(&url)
        .header("api-key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to scroll Qdrant");
            return vec![];
        }
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

    let cutoff = Utc::now() - chrono::Duration::days(VERIFY_STALE_DAYS);

    points
        .iter()
        .filter_map(|p| {
            let id = p.get("id").map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })?;
            let payload = p.get("payload")?;
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string();
            let content = payload.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let last_verified = payload
                .get("last_verified_at")
                .and_then(|v| v.as_str())
                .map(String::from);

            // Filter: only include memories not verified recently
            let needs_verify = match &last_verified {
                Some(ts) => {
                    match chrono::DateTime::parse_from_rfc3339(ts) {
                        Ok(dt) => dt.with_timezone(&Utc) < cutoff,
                        Err(_) => true,
                    }
                }
                None => true, // Never verified
            };

            if !needs_verify {
                return None;
            }

            Some(MemoryPoint {
                id,
                name,
                content,
                last_verified_at: last_verified,
            })
        })
        .take(MAX_VERIFY_PER_SESSION)
        .collect()
}

/// Extract claims from content using Claude Haiku (fast, cheap).
async fn extract_claims_claude(
    client: &reqwest::Client,
    api_key: &str,
    content: &str,
) -> Vec<Claim> {
    let prompt = format!(
        r#"Extract verifiable claims from this text. Return a JSON array of objects with:
- "claim_type": one of "file_path", "url", "port", "linear_issue", "version", "count", "status"
- "claim_text": what the text claims (short)
- "verifiable_value": the specific checkable value (the path, URL, port number, issue ID, etc)

Only extract claims that can be mechanically verified. Skip opinions, descriptions, and subjective statements.

Text:
{content}

Return ONLY the JSON array, no other text."#
    );

    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 2000,
        "messages": [{"role": "user", "content": prompt}]
    });

    let resp = match client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Claude API request failed");
            return vec![];
        }
    };

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(_) => return vec![],
    };

    // Claude Messages API response format
    let text = json
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("[]");

    // Strip markdown code fences
    let cleaned = text.trim();
    let cleaned = if cleaned.starts_with("```") {
        let inner = cleaned
            .trim_start_matches("```json")
            .trim_start_matches("```");
        inner.trim_end_matches("```").trim()
    } else {
        cleaned
    };

    serde_json::from_str(cleaned).unwrap_or_else(|e| {
        debug!(error = %e, "Failed to parse claims from Cerebras");
        vec![]
    })
}

/// Verify file_path claims with fs::exists(). Returns (verified, stale_reasons).
fn verify_claims(claims: &[Claim]) -> (bool, Vec<String>) {
    let mut stale_reasons = Vec::new();
    let mut any_stale = false;

    for claim in claims {
        if claim.claim_type != "file_path" {
            continue; // Only verify file paths in the hook (fast + no network)
        }

        let path = &claim.verifiable_value;

        // Try absolute path
        if std::path::Path::new(path).exists() {
            continue;
        }

        // Try expanding ~
        if path.starts_with("~/") || path.starts_with("~\\") {
            if let Some(home) = dirs::home_dir() {
                let expanded = home.join(&path[2..]);
                if expanded.exists() {
                    continue;
                }
            }
        }

        // File not found = stale
        any_stale = true;
        stale_reasons.push(format!("File not found: {path}"));
    }

    (!any_stale || stale_reasons.is_empty(), stale_reasons)
}

/// Update Qdrant payload with verification results.
async fn update_payload(
    client: &reqwest::Client,
    config: &QdrantConfig,
    point_id: &str,
    verified: bool,
    stale_reason: Option<&str>,
) {
    let now = Utc::now().to_rfc3339();

    let mut payload = serde_json::json!({
        "verified": verified,
        "last_verified_at": now
    });

    if let Some(reason) = stale_reason {
        payload["stale_reason"] = serde_json::Value::String(reason.to_string());
    } else {
        payload["stale_reason"] = serde_json::Value::String(String::new());
    }

    let body = serde_json::json!({
        "payload": payload,
        "points": [point_id]
    });

    let url = format!(
        "{}/collections/{}/points/payload",
        config.cluster_url, config.collection
    );

    let _ = client
        .post(&url)
        .header("api-key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;
}

/// Process SessionStart — verify stale memories.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // 1. Check 24h cooldown
    if !check_cooldown() {
        debug!("Memory verify cooldown active — skipping");
        return HookOutput::allow();
    }

    // 2. Load configs
    let qdrant_config = match load_qdrant_config() {
        Some(c) => c,
        None => {
            debug!("No Qdrant config — skipping memory verify");
            return HookOutput::allow();
        }
    };

    let anthropic_key = match load_anthropic_key() {
        Some(k) => k,
        None => {
            debug!("No Anthropic API key — skipping memory verify");
            return HookOutput::allow();
        }
    };

    // 3. Run async verification — handle both standalone and nested-runtime cases
    let stale_count = run_async(async {
        let client = match reqwest::Client::builder()
            .timeout(constants::API_CALL_TIMEOUT_LONG)
            .build()
        {
            Ok(c) => c,
            Err(_) => return 0usize,
        };

        // 4. Scroll for unverified memories
        let memories = scroll_unverified(&client, &qdrant_config).await;
        if memories.is_empty() {
            debug!("No memories need verification");
            return 0;
        }

        debug!(count = memories.len(), "Verifying memories");

        let mut stale = 0usize;

        // 5. Verify each memory
        for memory in &memories {
            let claims =
                extract_claims_claude(&client, &anthropic_key, &memory.content).await;

            if claims.is_empty() {
                // No claims — mark as verified (nothing to disprove)
                update_payload(&client, &qdrant_config, &memory.id, true, None).await;
                continue;
            }

            let (all_ok, reasons) = verify_claims(&claims);

            if all_ok {
                update_payload(&client, &qdrant_config, &memory.id, true, None).await;
            } else {
                let reason = reasons.join("; ");
                update_payload(
                    &client,
                    &qdrant_config,
                    &memory.id,
                    false,
                    Some(&reason),
                )
                .await;
                stale += 1;
                debug!(name = %memory.name, reason = %reason, "Memory flagged as stale");
            }
        }

        stale
    });

    // 6. Write cooldown
    write_cooldown();

    // 7. Inject context if stale memories found
    if stale_count > 0 {
        let msg = format!(
            "[Qdrant Memory] {} memories flagged as potentially stale",
            stale_count
        );
        // SessionStart context injection must use the event that supports it
        let _ = input; // suppress unused warning
        return HookOutput::inject_context(HookEvent::SessionStart, &msg);
    }

    HookOutput::allow()
}
