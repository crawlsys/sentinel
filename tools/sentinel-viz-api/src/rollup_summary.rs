//! Rollup-summary endpoint — 5-10 word blurbs for `×N`
//! EventTicker rollups (e.g. "edited EventTicker, ran tests,
//! pushed").
//!
//! WORKSTREAM: sentinel-viz — internal to this crate.
//!
//! ## Why a dedicated endpoint
//!
//! The viz already has `/api/summary/{session_id}` for
//! card/wait/narrative renders, but those:
//!   - operate on full session transcripts (heavy)
//!   - have multi-paragraph output budgets (verbose)
//!   - are keyed by session, not by rollup signature
//!
//! Rollup blurbs need the opposite: a tight 5-10 word phrase for
//! a small bag of tool calls inside ONE rolled row. The frontend
//! provides the rollup's members (tool name + optional
//! pre-computed summary line) so the LLM never sees raw
//! transcript text — keeps tokens tiny and latency low.
//!
//! ## Caching
//!
//! The frontend supplies a `cache_key` (a stable hash of the
//! rollup signature; the EventTicker computes this client-side
//! so the same `×9 Bash` rollup across re-renders maps to one
//! entry). The server caches keyed on that — no LLM call when
//! the same rollup is re-requested.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::llm::{chat, ChatRequest, ModelConfig};

/// Cache TTL. Rollups age out of operator interest fast — once a
/// session moves on, the cached blurb is rarely re-fetched. 15
/// min covers a long working session without unbounded growth.
const ROLLUP_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// LLM budget. 60 tokens covers "edited EventTicker.tsx, ran
/// vitest, pushed b6d1992" comfortably and leaves headroom for
/// the model to format a clean phrase.
const MAX_TOKENS: u32 = 60;

/// Soft response timeout. The frontend renders "(summarizing…)"
/// while the request is in flight; if the model exceeds this, the
/// blurb is dropped (frontend stays on the "(summarizing…)"
/// placeholder until the next render evicts it). 20s is enough
/// for a cold-load on a 30B model the first time.
const TIMEOUT_SECS: u64 = 20;

const SYSTEM_PROMPT: &str = "\
You are summarizing a small cluster of AI-agent tool calls into a single \
5-10 word phrase suitable for a dashboard rollup label. Output ONLY the \
phrase — no quotes, no trailing period, no explanation. Be concrete: name \
files and verbs (\"edited EventTicker, ran tests, pushed\") rather than \
abstractions (\"performed development tasks\"). Match the operator's \
present-tense imperative voice.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupMember {
    /// Tool name, e.g. "Bash", "Edit", "Read".
    pub tool: String,
    /// Optional pre-extracted summary from the activity-cache
    /// (typically the compactSummaryFor output — Bash command
    /// verb, file basename, etc.). Empty when the cache hadn't
    /// warmed yet on the client.
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Deserialize)]
pub struct RollupRequest {
    /// Stable identifier for this rollup across re-renders.
    /// Client computes from the rollup's signature so the cache
    /// hits on subsequent fetches.
    pub cache_key: String,
    pub session_id: String,
    pub members: Vec<RollupMember>,
}

#[derive(Debug, Serialize)]
pub struct RollupResponse {
    pub cache_key: String,
    pub session_id: String,
    /// 5-10 word blurb, or `None` when the LLM is unavailable
    /// (no model configured, rate-limited, request failed).
    /// Frontend keeps showing its existing label/preview when
    /// `None`.
    pub summary: Option<String>,
    /// Diagnostic — "cache", "llm:<model_label>", "no-model",
    /// "no-members", "rate-limited", "llm-error".
    pub source: String,
    pub cached: bool,
}

/// Per-cache_key entry. `built_at` drives TTL eviction; the
/// stored summary may be `None` to NEGATIVELY cache LLM
/// failures for a short window so a wedged model doesn't get
/// hammered.
#[derive(Clone)]
struct CacheEntry {
    summary: Option<String>,
    built_at: Instant,
}

pub struct RollupState {
    /// Shared LLM config with the naming + summary endpoints.
    /// Wrapped in RwLock for hot-swap via /api/config — the same
    /// pattern naming.rs uses.
    pub model: RwLock<Option<ModelConfig>>,
    cache: RwLock<HashMap<String, CacheEntry>>,
}

impl RollupState {
    pub async fn from_env() -> Self {
        Self {
            model: RwLock::new(ModelConfig::from_env().await),
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn set_model(&self, m: Option<ModelConfig>) {
        *self.model.write().unwrap() = m;
        // Drop cache on model swap — a different model would
        // give different blurbs.
        self.cache.write().unwrap().clear();
    }

    fn cached(&self, key: &str) -> Option<Option<String>> {
        let cache = self.cache.read().unwrap();
        let entry = cache.get(key)?;
        if entry.built_at.elapsed() > ROLLUP_CACHE_TTL {
            return None;
        }
        Some(entry.summary.clone())
    }

    fn store(&self, key: &str, summary: Option<String>) {
        let mut cache = self.cache.write().unwrap();
        cache.insert(
            key.to_string(),
            CacheEntry { summary, built_at: Instant::now() },
        );
    }
}

/// Compose the prompt input — one line per member, dense and
/// deduplicated so the LLM doesn't waste tokens on five
/// identical Bash entries.
fn format_blob(members: &[RollupMember]) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for m in members {
        let key = format!("{}|{}", m.tool, m.summary);
        if !seen.insert(key) {
            continue;
        }
        if m.summary.is_empty() {
            lines.push(m.tool.clone());
        } else {
            lines.push(format!("{}: {}", m.tool, m.summary));
        }
    }
    lines.join("\n")
}

/// Strip the model's response down to a clean blurb. Trims
/// whitespace, drops surrounding quotes the LLM occasionally
/// adds despite the system prompt, and caps to 100 chars
/// (well above 10 words of normal text) as a defensive bound.
fn sanitize(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if trimmed.is_empty() {
        return None;
    }
    // Take the first line if the model wandered.
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    let bounded = if first_line.len() > 100 {
        first_line.chars().take(100).collect::<String>()
    } else {
        first_line.to_string()
    };
    Some(bounded)
}

/// Produce a rollup summary, hitting cache first and the LLM
/// only when missing. Always succeeds — every error path is
/// internalised, with the failure reason carried in
/// `RollupResponse.source`.
pub async fn summarize(state: &RollupState, req: RollupRequest) -> RollupResponse {
    if req.members.is_empty() {
        return RollupResponse {
            cache_key: req.cache_key,
            session_id: req.session_id,
            summary: None,
            source: "no-members".to_string(),
            cached: false,
        };
    }

    if let Some(cached) = state.cached(&req.cache_key) {
        return RollupResponse {
            cache_key: req.cache_key,
            session_id: req.session_id,
            summary: cached,
            source: "cache".to_string(),
            cached: true,
        };
    }

    let model_snapshot = state.model.read().unwrap().clone();
    let Some(model) = model_snapshot.as_ref() else {
        // No model configured — don't cache (so a later
        // /api/config POST that wires a model can fill in
        // immediately).
        return RollupResponse {
            cache_key: req.cache_key,
            session_id: req.session_id,
            summary: None,
            source: "no-model".to_string(),
            cached: false,
        };
    };

    let blob = format_blob(&req.members);
    let result: Result<String> = chat(
        model,
        ChatRequest {
            system: SYSTEM_PROMPT,
            user: &blob,
            max_tokens: MAX_TOKENS,
            temperature: 0.3,
            timeout_secs: TIMEOUT_SECS,
        },
    )
    .await;

    let (summary, source) = match result {
        Ok(s) => match sanitize(&s) {
            Some(text) => (Some(text), format!("llm:{}", model.label())),
            None => (None, "llm-error".to_string()),
        },
        Err(e) => {
            tracing::warn!(error = %e, "rollup summary LLM call failed");
            (None, "llm-error".to_string())
        }
    };

    // Cache positive AND negative results — keeps wedged LLMs
    // from getting hammered. The TTL is short enough (15 min)
    // that a recovered backend re-summarises soon enough.
    state.store(&req.cache_key, summary.clone());

    RollupResponse {
        cache_key: req.cache_key,
        session_id: req.session_id,
        summary,
        source,
        cached: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_blob_dedupes_identical_members() {
        let members = vec![
            RollupMember { tool: "Bash".into(), summary: "cargo test".into() },
            RollupMember { tool: "Bash".into(), summary: "cargo test".into() },
            RollupMember { tool: "Bash".into(), summary: "git status".into() },
        ];
        let blob = format_blob(&members);
        // Two unique lines, not three.
        assert_eq!(blob.lines().count(), 2);
        assert!(blob.contains("cargo test"));
        assert!(blob.contains("git status"));
    }

    #[test]
    fn format_blob_falls_back_to_bare_tool_when_no_summary() {
        let members = vec![
            RollupMember { tool: "Bash".into(), summary: String::new() },
            RollupMember { tool: "Read".into(), summary: "x.ts".into() },
        ];
        let blob = format_blob(&members);
        assert!(blob.contains("Bash"));
        assert!(blob.contains("Read: x.ts"));
    }

    #[test]
    fn sanitize_strips_quotes_and_truncates() {
        assert_eq!(sanitize("\"edited file\""), Some("edited file".into()));
        assert_eq!(sanitize("  ran tests  "), Some("ran tests".into()));
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("   "), None);
        // Multi-line input takes the first line.
        assert_eq!(sanitize("first\nsecond"), Some("first".into()));
        // Length cap.
        let long = "a".repeat(200);
        let out = sanitize(&long).unwrap();
        assert!(out.len() <= 100);
    }

    #[tokio::test]
    async fn empty_members_returns_no_members_source() {
        let state = RollupState {
            model: RwLock::new(None),
            cache: RwLock::new(HashMap::new()),
        };
        let req = RollupRequest {
            cache_key: "test-k".into(),
            session_id: "sess-x".into(),
            members: vec![],
        };
        let resp = summarize(&state, req).await;
        assert_eq!(resp.source, "no-members");
        assert!(resp.summary.is_none());
    }

    #[tokio::test]
    async fn no_model_returns_no_model_source_and_does_not_cache() {
        let state = RollupState {
            model: RwLock::new(None),
            cache: RwLock::new(HashMap::new()),
        };
        let req = RollupRequest {
            cache_key: "test-k".into(),
            session_id: "sess-x".into(),
            members: vec![RollupMember { tool: "Bash".into(), summary: "ls".into() }],
        };
        let resp = summarize(&state, req).await;
        assert_eq!(resp.source, "no-model");
        assert!(resp.summary.is_none());
        assert!(!resp.cached);
        // Critically: cache is empty so a follow-up call after
        // model is wired will fire the LLM, not return None from
        // cache.
        assert!(state.cache.read().unwrap().is_empty());
    }
}
