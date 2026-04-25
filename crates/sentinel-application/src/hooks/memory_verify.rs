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
use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest, VectorStorePort};
use std::path::PathBuf;
use tracing::{debug, warn};

use super::{run_async, FileSystemPort, HookContext};

/// Maximum memories to verify per session to limit API calls.
const MAX_VERIFY_PER_SESSION: usize = constants::MAX_VERIFY_PER_SESSION;

/// Memories not verified in this many days are eligible for re-verification.
const VERIFY_STALE_DAYS: i64 = constants::VERIFY_STALE_DAYS;

/// Qdrant collection shared with memory_inject / memory_extract / memory_feedback.
const COLLECTION: &str = "claude-memory";

/// 24h cooldown file path (via FileSystemPort).
fn cooldown_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir().map(|h| {
        h.join(".claude")
            .join("sentinel")
            .join("state")
            .join("last-verify.txt")
    })
}

/// Check if 24h cooldown has elapsed.
fn check_cooldown(fs: &dyn FileSystemPort) -> bool {
    let path = match cooldown_path(fs) {
        Some(p) => p,
        None => return true,
    };
    let content = match fs.read_to_string(&path) {
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
fn write_cooldown(fs: &dyn FileSystemPort) {
    let path = match cooldown_path(fs) {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let _ = fs.write(&path, Utc::now().to_rfc3339().as_bytes());
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

/// Scroll Qdrant for memories not verified in the last N days (via VectorStorePort).
async fn scroll_unverified(vector_store: &dyn VectorStorePort) -> Vec<MemoryPoint> {
    let results = match vector_store.scroll(COLLECTION, None, 100).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to scroll Qdrant");
            return vec![];
        }
    };

    let cutoff = Utc::now() - chrono::Duration::days(VERIFY_STALE_DAYS);

    results
        .into_iter()
        .filter_map(|p| {
            let id = p.id;
            let payload = &p.payload;
            let name = payload
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unnamed")
                .to_string();
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let last_verified = payload
                .get("last_verified_at")
                .and_then(|v| v.as_str())
                .map(String::from);

            let needs_verify = match &last_verified {
                Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
                    Ok(dt) => dt.with_timezone(&Utc) < cutoff,
                    Err(_) => true,
                },
                None => true,
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

/// Extract claims from content using the LLM port (Claude Haiku).
async fn extract_claims(llm: &dyn LlmPort, content: &str) -> Vec<Claim> {
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

    let text = match llm
        .complete(LlmRequest {
            model: LlmModel::Haiku,
            prompt,
            max_tokens: 2000,
        })
        .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "LLM claim extraction failed");
            return vec![];
        }
    };

    // Strip markdown code fences (the model sometimes wraps JSON in ```json ... ```).
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
        debug!(error = %e, "Failed to parse claims from LLM");
        vec![]
    })
}

/// Verify file_path claims with `fs.exists()` via FileSystemPort.
/// Returns (verified, stale_reasons).
fn verify_claims(fs: &dyn FileSystemPort, claims: &[Claim]) -> (bool, Vec<String>) {
    let mut stale_reasons = Vec::new();
    let mut any_stale = false;

    for claim in claims {
        if claim.claim_type != "file_path" {
            continue; // Only verify file paths in the hook (fast + no network)
        }

        let path = &claim.verifiable_value;

        // Try absolute path
        if fs.exists(std::path::Path::new(path)) {
            continue;
        }

        // Try expanding ~
        if path.starts_with("~/") || path.starts_with("~\\") {
            if let Some(home) = fs.home_dir() {
                let expanded = home.join(&path[2..]);
                if fs.exists(&expanded) {
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

/// Update Qdrant payload with verification results (via VectorStorePort).
async fn update_payload(
    vector_store: &dyn VectorStorePort,
    point_id: &str,
    verified: bool,
    stale_reason: Option<&str>,
) {
    let now = Utc::now().to_rfc3339();

    let reason_str = stale_reason.unwrap_or("").to_string();
    let payload = serde_json::json!({
        "verified": verified,
        "last_verified_at": now,
        "stale_reason": reason_str,
    });

    let ids = [point_id.to_string()];
    if let Err(e) = vector_store.set_payload(COLLECTION, &ids, payload).await {
        debug!(error = %e, "memory_verify set_payload failed");
    }
}

/// Process SessionStart — verify stale memories.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // 1. Check 24h cooldown
    if !check_cooldown(ctx.fs) {
        debug!("Memory verify cooldown active — skipping");
        return HookOutput::allow();
    }

    // 2. Require a configured vector store (Qdrant config now owned by the
    //    infrastructure adapter — no more local ~/.qdrant/config.json read).
    let vector_store = match ctx.vector_store {
        Some(vs) => vs,
        None => {
            debug!("No vector store configured — skipping memory verify");
            return HookOutput::allow();
        }
    };

    // 3. LLM port — required for claim extraction. Skip silently if not
    //    wired (e.g. no ANTHROPIC_API_KEY in env).
    let llm = match ctx.llm {
        Some(l) => l,
        None => {
            debug!("No LLM port configured — skipping memory verify");
            return HookOutput::allow();
        }
    };

    let fs = ctx.fs;

    // 4. Run async verification.
    let stale_count = run_async(async {
        let memories = scroll_unverified(vector_store).await;
        if memories.is_empty() {
            debug!("No memories need verification");
            return 0;
        }

        debug!(count = memories.len(), "Verifying memories");

        let mut stale = 0usize;

        for memory in &memories {
            let claims = extract_claims(llm, &memory.content).await;

            if claims.is_empty() {
                // No claims — mark as verified (nothing to disprove)
                update_payload(vector_store, &memory.id, true, None).await;
                continue;
            }

            let (all_ok, reasons) = verify_claims(fs, &claims);

            if all_ok {
                update_payload(vector_store, &memory.id, true, None).await;
            } else {
                let reason = reasons.join("; ");
                update_payload(vector_store, &memory.id, false, Some(&reason)).await;
                stale += 1;
                debug!(name = %memory.name, reason = %reason, "Memory flagged as stale");
            }
        }

        stale
    });

    // 5. Write cooldown
    write_cooldown(ctx.fs);

    // 6. Inject context if stale memories found
    if stale_count > 0 {
        let msg = format!(
            "[Qdrant Memory] {} memories flagged as potentially stale",
            stale_count
        );
        let _ = input; // suppress unused warning
        return HookOutput::inject_context(HookEvent::SessionStart, &msg);
    }

    HookOutput::allow()
}
