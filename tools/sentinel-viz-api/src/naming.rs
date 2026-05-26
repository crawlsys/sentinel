//! Session naming. Replaces UUID slices with a 3-word human-readable
//! label produced by an LLM. Configurable via `SENTINEL_VIZ_NAMING_MODEL`:
//!
//!   none                   — no naming, callers fall back to UUID slice
//!   openai:gpt-4o-mini     — `OpenAI` Chat Completions API (uses `OPENAI_API_KEY`)
//!   openai:gpt-4o          — same, larger model
//!   local:<model>          — Ollama at `OLLAMA_URL` (default <http://127.0.0.1:11434>)
//!
//! Constraints:
//!   - Rate-limited: max 10 outbound LLM calls per minute
//!   - Cached: 24h TTL keyed by `session_id` + first-prompt-sha
//!   - Graceful: any error → return None, caller uses UUID slice
//!   - Lazy: only called when the frontend asks (no eager fan-out)
//!
//! WORKSTREAM: sentinel-viz — internal to this crate; the LLM call
//! crosses out to OpenAI/Ollama but that's NOT a Sentinel boundary,
//! it's a third-party API choice the operator configures.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::llm::{self, ModelConfig};
use crate::transcript;

const CACHE_TTL_SECS: u64 = 24 * 3600;
const RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_MAX_CALLS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NameResponse {
    pub session_id: String,
    /// `None` when naming is disabled, the LLM is unreachable, or
    /// the model returned a useless response. Callers fall back to
    /// the UUID slice.
    pub name: Option<String>,
    pub source: String,
    pub cached: bool,
}

/// Result of a cache probe. Distinguishes a miss from a hit that
/// cached a `None` name (the LLM produced nothing usable) so callers
/// don't re-issue an LLM call for a known-empty result.
enum CacheLookup {
    Hit(Option<String>),
    Miss,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    name: Option<String>,
    built_at: Instant,
    /// SHA hash (first 16 hex) of the first user prompt — invalidates
    /// the cache if the session's first prompt changes (effectively
    /// never, but a guarded invariant).
    prompt_fp: String,
}

pub struct NamingState {
    cache: RwLock<HashMap<String, CacheEntry>>,
    /// Outbound-call timestamps within the rate window. Cheap O(N).
    recent_calls: RwLock<Vec<Instant>>,
    /// Mutable so the /api/config endpoint can hot-swap without restart.
    pub model: RwLock<Option<ModelConfig>>,
}

impl NamingState {
    pub fn from_env() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            recent_calls: RwLock::new(Vec::new()),
            model: RwLock::new(ModelConfig::from_env()),
        }
    }

    pub fn set_model(&self, m: Option<ModelConfig>) {
        *self.model.write().unwrap_or_else(std::sync::PoisonError::into_inner) = m;
        // Drop cache so a new model can re-name everything.
        self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }

    /// Decide whether the rate limiter would allow another call.
    /// Side-effect-free.
    fn rate_allowed(&self) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        let calls = self.recent_calls.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        let in_window = calls.iter().filter(|t| now.duration_since(**t) < window).count();
        in_window < RATE_LIMIT_MAX_CALLS
    }

    fn record_call(&self) {
        let now = Instant::now();
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        let mut calls = self.recent_calls.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.retain(|t| now.duration_since(*t) < window);
        calls.push(now);
    }

    fn cached(&self, session_id: &str, fp: &str) -> CacheLookup {
        let cache = self.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = cache.get(session_id) else { return CacheLookup::Miss };
        if entry.prompt_fp != fp {
            return CacheLookup::Miss;
        }
        if entry.built_at.elapsed().as_secs() > CACHE_TTL_SECS {
            return CacheLookup::Miss;
        }
        CacheLookup::Hit(entry.name.clone())
    }

    fn store(&self, session_id: &str, name: Option<String>, fp: String) {
        let mut cache = self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            session_id.to_string(),
            CacheEntry {
                name,
                built_at: Instant::now(),
                prompt_fp: fp,
            },
        );
    }
}

/// Produce a 3-word name for a session, or `None` for graceful
/// fallback. Always succeeds — every error path is internalised.
pub async fn name_session(state: &NamingState, session_id: &str) -> NameResponse {
    let no_model = NameResponse {
        session_id: session_id.to_string(),
        name: None,
        source: "disabled".to_string(),
        cached: false,
    };
    let model_snapshot = state.model.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let Some(model) = model_snapshot.as_ref() else { return no_model };

    // Build the prompt input — first user message + first 3 tool
    // calls from the JSONL transcript.
    let Some(path) = transcript::find_transcript(session_id) else {
        return NameResponse {
            session_id: session_id.to_string(),
            name: None,
            source: "no-transcript".to_string(),
            cached: false,
        };
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return NameResponse {
            session_id: session_id.to_string(),
            name: None,
            source: "read-error".to_string(),
            cached: false,
        };
    };
    let (first_prompt, first_tools) = extract_seeds(&content);
    if first_prompt.is_empty() {
        return NameResponse {
            session_id: session_id.to_string(),
            name: None,
            source: "no-prompt".to_string(),
            cached: false,
        };
    }
    let fp = fingerprint(&first_prompt);

    if let CacheLookup::Hit(cached) = state.cached(session_id, &fp) {
        return NameResponse {
            session_id: session_id.to_string(),
            name: cached,
            source: "cache".to_string(),
            cached: true,
        };
    }

    if !state.rate_allowed() {
        return NameResponse {
            session_id: session_id.to_string(),
            name: None,
            source: "rate-limited".to_string(),
            cached: false,
        };
    }
    state.record_call();

    let prompt_blob = format_prompt_blob(&first_prompt, &first_tools);
    let name = match llm::chat(
        model,
        llm::ChatRequest {
            system: SYSTEM_PROMPT,
            user: &prompt_blob,
            max_tokens: 20,
            temperature: 0.3,
            timeout_secs: 15,
        },
    )
    .await
    {
        Ok(s) => sanitize_name(&s),
        Err(e) => {
            tracing::warn!(error = %e, "naming LLM call failed");
            None
        }
    };

    state.store(session_id, name.clone(), fp);

    NameResponse {
        session_id: session_id.to_string(),
        name,
        source: model.label(),
        cached: false,
    }
}

/// Pull the first user prompt + the first N tool-call names+args.
fn extract_seeds(jsonl: &str) -> (String, Vec<String>) {
    let mut first_user = String::new();
    let mut tools: Vec<String> = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(line) else { continue };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if typ == "user" && first_user.is_empty() {
            let content = v.get("message").and_then(|m| m.get("content"));
            if let Some(text) = content.and_then(|c| c.as_str()) {
                let trimmed = text.trim();
                // Skip Claude Code's injected envelope blocks.
                if !trimmed.is_empty()
                    && !trimmed.starts_with('<')
                    && !trimmed.starts_with("Caveat:")
                {
                    first_user = trimmed.chars().take(800).collect();
                }
            }
        }
        if typ == "assistant" && tools.len() < 3 {
            if let Some(blocks) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                for b in blocks {
                    if tools.len() >= 3 {
                        break;
                    }
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        let inp = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
                        let summary = crate::activity::tool_summary(name, &inp);
                        tools.push(format!("{name}: {summary}"));
                    }
                }
            }
        }
        if !first_user.is_empty() && tools.len() >= 3 {
            break;
        }
    }
    (first_user, tools)
}

fn fingerprint(prompt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn format_prompt_blob(first_prompt: &str, tools: &[String]) -> String {
    let mut s = String::new();
    s.push_str("First user prompt:\n");
    s.push_str(first_prompt);
    if !tools.is_empty() {
        s.push_str("\n\nFirst tool calls:\n");
        for t in tools {
            s.push_str("- ");
            s.push_str(t);
            s.push('\n');
        }
    }
    s
}

const SYSTEM_PROMPT: &str = "You produce concise 2-3 word labels naming what a Claude Code session is working on. Return ONLY the label — no quotes, no punctuation, no commentary. Examples: 'viz rewrite', 'scoop watchdog', 'auth migration'. Stay under 3 words.";

pub fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == ',' || c == '`')
        .to_string();
    if cleaned.is_empty() {
        return None;
    }
    // Hard cap to 3 words.
    let words: Vec<&str> = cleaned.split_whitespace().take(3).collect();
    if words.is_empty() {
        return None;
    }
    let out = words.join(" ").to_lowercase();
    // Bail on obvious model fabrications: parens, refusals, etc.
    if out.contains("i cannot") || out.contains("i'm sorry") || out.contains('[') {
        return None;
    }
    Some(out)
}

#[cfg(test)]
#[allow(unsafe_code)] // env mutation in from_env_none_means_no_naming
mod tests {
    use super::*;

    #[test]
    fn sanitize_caps_to_three_words() {
        assert_eq!(sanitize_name("viz rewrite phase two extra").as_deref(), Some("viz rewrite phase"));
    }

    #[test]
    fn sanitize_strips_quotes_and_punct() {
        assert_eq!(sanitize_name("\"Scoop watchdog.\"").as_deref(), Some("scoop watchdog"));
    }

    #[test]
    fn sanitize_rejects_refusals() {
        assert!(sanitize_name("I cannot determine").is_none());
        assert!(sanitize_name("I'm sorry").is_none());
    }

    #[test]
    fn sanitize_handles_empty() {
        assert!(sanitize_name("").is_none());
        assert!(sanitize_name("   ").is_none());
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint("hello"), fingerprint("hello"));
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }

    #[test]
    fn from_env_none_means_no_naming() {
        // SAFETY: tests in this file are not parallelised; this is fine.
        unsafe {
            std::env::remove_var("SENTINEL_VIZ_NAMING_MODEL");
        }
        let s = NamingState::from_env();
        assert!(s.model.read().unwrap_or_else(std::sync::PoisonError::into_inner).is_none());
    }
}
