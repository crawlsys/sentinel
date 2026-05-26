//! Card / wait / narrative summaries via LLM.
//!
//! Three kinds of summary the UI can ask for:
//!
//!   - `card`     : 2-4 sentence recap of what an agent did in a session
//!                  window (anchored by `at_ts` ± 45s if given, else the
//!                  most recent activity tail). Shown above the
//!                  activity-segment list in the inspector.
//!   - `wait`     : 1-2 sentences explaining what an `awaiting_user`
//!                  session is blocked on. Shown in the STUCK panel.
//!   - `narrative`: 1-line "what's happening right now" across N
//!                  sessions in the last M seconds. Shown in a future
//!                  bottom-console live feed (not wired yet).
//!
//! Uses the same `SENTINEL_VIZ_NAMING_MODEL` knob as session naming.
//! Cached server-side (10-min TTL) keyed by inputs. Rate-limited
//! globally to 12 LLM calls/min across all summary kinds.
//!
//! WORKSTREAM: sentinel-viz — internal; the LLM call is third-party.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::activity;
use crate::llm::{self, ModelConfig};
use crate::model::ActivityResponse;

const CACHE_TTL_SECS: u64 = 10 * 60;
const RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_MAX_CALLS: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryResponse {
    pub session_id: String,
    pub kind: String,
    pub at_ts: Option<String>,
    /// None = not generated (disabled, rate-limited, errored, or
    /// insufficient activity). Caller hides the card or falls back.
    pub text: Option<String>,
    pub source: String,
    pub cached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SummaryKind {
    Card,
    Wait,
    Narrative,
}

impl SummaryKind {
    /// Parse the wire string (`card` / `wait` / `narrative`). Named
    /// `parse` rather than `from_str` to avoid shadowing the standard
    /// `FromStr` trait method.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "card" => Some(Self::Card),
            "wait" => Some(Self::Wait),
            "narrative" => Some(Self::Narrative),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::Wait => "wait",
            Self::Narrative => "narrative",
        }
    }

    const fn system_prompt(self) -> &'static str {
        match self {
            Self::Card => "You summarize what an autonomous coding agent did in a Sentinel session window. Be terse, concrete, focused on what the agent decided and what state it produced. 2-4 short sentences. No preamble. No markdown headers or bullet lists.",
            Self::Wait => "You explain in 1-2 sentences what an autonomous coding agent is blocked on, based on its last messages and pending tool call. The operator needs to know what to do to unblock it. No preamble.",
            Self::Narrative => "You produce a 1-sentence headline of what's happening across multiple autonomous coding agents right now. No preamble, no quotes.",
        }
    }
}

pub struct SummaryState {
    pub model: RwLock<Option<ModelConfig>>,
    /// (`session_id`, kind, `at_ts`) → cached response.
    cache: RwLock<HashMap<String, CacheEntry>>,
    recent_calls: RwLock<Vec<Instant>>,
}

/// Result of a cache probe. Distinguishes a miss from a hit that
/// cached a `None` summary (no usable text) so callers don't re-issue
/// an LLM call for a known-empty result.
enum CacheLookup {
    Hit(Option<String>),
    Miss,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    text: Option<String>,
    built_at: Instant,
}

impl SummaryState {
    pub fn from_env() -> Self {
        Self {
            model: RwLock::new(ModelConfig::from_env()),
            cache: RwLock::new(HashMap::new()),
            recent_calls: RwLock::new(Vec::new()),
        }
    }

    pub fn set_model(&self, m: Option<ModelConfig>) {
        *self.model.write().unwrap_or_else(std::sync::PoisonError::into_inner) = m;
        self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }

    fn rate_allowed(&self) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        let calls = self.recent_calls.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.iter().filter(|t| now.duration_since(**t) < window).count() < RATE_LIMIT_MAX_CALLS
    }

    fn record_call(&self) {
        let now = Instant::now();
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        let mut calls = self.recent_calls.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.retain(|t| now.duration_since(*t) < window);
        calls.push(now);
    }

    fn key(session_id: &str, kind: SummaryKind, at_ts: Option<&str>) -> String {
        format!("{}|{}|{}", session_id, kind.label(), at_ts.unwrap_or(""))
    }

    fn cached(&self, key: &str) -> CacheLookup {
        let cache = self.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(e) = cache.get(key) else { return CacheLookup::Miss };
        if e.built_at.elapsed().as_secs() > CACHE_TTL_SECS {
            return CacheLookup::Miss;
        }
        CacheLookup::Hit(e.text.clone())
    }

    fn store(&self, key: &str, text: Option<String>) {
        let mut cache = self.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(key.to_string(), CacheEntry { text, built_at: Instant::now() });
    }
}

pub async fn summarize(
    state: &SummaryState,
    session_id: &str,
    kind: SummaryKind,
    at_ts: Option<&str>,
) -> SummaryResponse {
    let disabled = SummaryResponse {
        session_id: session_id.to_string(),
        kind: kind.label().to_string(),
        at_ts: at_ts.map(std::string::ToString::to_string),
        text: None,
        source: "disabled".to_string(),
        cached: false,
    };
    let model_snapshot = state.model.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let Some(model) = model_snapshot.as_ref() else { return disabled };

    let key = SummaryState::key(session_id, kind, at_ts);
    if let CacheLookup::Hit(cached) = state.cached(&key) {
        return SummaryResponse {
            session_id: session_id.to_string(),
            kind: kind.label().to_string(),
            at_ts: at_ts.map(std::string::ToString::to_string),
            text: cached,
            source: "cache".to_string(),
            cached: true,
        };
    }
    if !state.rate_allowed() {
        return SummaryResponse {
            session_id: session_id.to_string(),
            kind: kind.label().to_string(),
            at_ts: at_ts.map(std::string::ToString::to_string),
            text: None,
            source: "rate-limited".to_string(),
            cached: false,
        };
    }

    // Gather context. For card+wait we read the session's activity
    // segments (already JSONL-derived).
    let limit = match kind {
        SummaryKind::Card => 60,
        SummaryKind::Wait | SummaryKind::Narrative => 30,
    };
    let window = match kind {
        SummaryKind::Card => 45,
        SummaryKind::Wait => 60,
        SummaryKind::Narrative => 30,
    };
    let activity_data = activity::session_activity(session_id, limit, at_ts, window);
    let blob = build_activity_blob(&activity_data, kind);
    if blob.trim().is_empty() {
        let resp = SummaryResponse {
            session_id: session_id.to_string(),
            kind: kind.label().to_string(),
            at_ts: at_ts.map(std::string::ToString::to_string),
            text: None,
            source: "no-activity".to_string(),
            cached: false,
        };
        return resp;
    }

    state.record_call();
    let max_tokens = match kind {
        SummaryKind::Card => 220,
        SummaryKind::Wait => 100,
        SummaryKind::Narrative => 60,
    };
    let text = match llm::chat(
        model,
        llm::ChatRequest {
            system: kind.system_prompt(),
            user: &blob,
            max_tokens,
            temperature: 0.3,
            timeout_secs: 20,
        },
    )
    .await
    {
        Ok(s) => sanitize(&s),
        Err(e) => {
            tracing::warn!(error = %e, kind = ?kind, "summary LLM call failed");
            None
        }
    };
    state.store(&key, text.clone());

    SummaryResponse {
        session_id: session_id.to_string(),
        kind: kind.label().to_string(),
        at_ts: at_ts.map(std::string::ToString::to_string),
        text,
        source: model.label(),
        cached: false,
    }
}

fn build_activity_blob(a: &ActivityResponse, kind: SummaryKind) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    // Writes to a String are infallible, so the `write!` results are
    // intentionally discarded.
    if let Some(t) = a.transcript.as_deref() {
        let _ = writeln!(out, "Session: {} (transcript {})", a.session_id, t);
    } else {
        let _ = writeln!(out, "Session: {}", a.session_id);
    }
    out.push_str("Activity (chronological):\n");
    for ev in &a.events {
        let ts = ev.ts.get(11..19).unwrap_or(&ev.ts);
        match ev.kind.as_str() {
            "tool_use" => {
                let tool = ev.tool.as_deref().unwrap_or("");
                let text = ev.text.as_deref().unwrap_or("");
                let _ = writeln!(out, "[{ts}] tool {tool}: {text}");
            }
            "tool_result" => {
                let text = ev.text.as_deref().unwrap_or("");
                let err = ev.is_error.unwrap_or(false);
                if err {
                    let _ = writeln!(out, "[{ts}] ↳ ERROR: {text}");
                } else {
                    let _ = writeln!(out, "[{ts}] ↳ {text}");
                }
            }
            "user" => {
                let _ = writeln!(out, "[{ts}] user: {}", ev.text.as_deref().unwrap_or(""));
            }
            "assistant" => {
                let _ = writeln!(out, "[{ts}] assistant: {}", ev.text.as_deref().unwrap_or(""));
            }
            other => {
                let _ = writeln!(out, "[{ts}] {other}: {}", ev.text.as_deref().unwrap_or(""));
            }
        }
    }
    if matches!(kind, SummaryKind::Wait) {
        out.push_str("\nThe operator needs to know what this session is blocked on and what to do next.\n");
    }
    out
}

fn sanitize(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Bail on obvious refusals.
    let low = trimmed.to_lowercase();
    if low.starts_with("i cannot") || low.starts_with("i'm sorry") || low.starts_with("i am sorry") {
        return None;
    }
    // Strip wrapping quotes.
    let cleaned = trimmed.trim_matches(|c: char| c == '"' || c == '\'');
    Some(cleaned.to_string())
}

/// Build a `SummaryState` that handles a missing model gracefully —
/// returns "disabled" without ever hitting the network.
#[cfg(test)]
pub fn disabled_for_tests() -> SummaryState {
    SummaryState {
        model: RwLock::new(None),
        cache: RwLock::new(HashMap::new()),
        recent_calls: RwLock::new(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_returns_no_text() {
        let s = disabled_for_tests();
        let r = summarize(&s, "sess-x", SummaryKind::Card, None).await;
        assert!(r.text.is_none());
        assert_eq!(r.source, "disabled");
    }

    #[test]
    fn sanitize_strips_quotes_and_rejects_refusals() {
        assert_eq!(sanitize("\"hello\"").as_deref(), Some("hello"));
        assert!(sanitize("I cannot summarize").is_none());
        assert!(sanitize("   ").is_none());
    }

    #[test]
    fn kind_from_str_round_trips() {
        for k in ["card", "wait", "narrative"] {
            let kind = SummaryKind::parse(k).unwrap();
            assert_eq!(kind.label(), k);
        }
        assert!(SummaryKind::parse("nope").is_none());
    }
}
