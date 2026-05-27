//! Session naming. Replaces UUID slices with a 3-word human-readable
//! label produced by an LLM. Configurable via `SENTINEL_VIZ_NAMING_MODEL`:
//!
//!   none                   — no naming, callers fall back to UUID slice
//!   openai:gpt-4o-mini     — OpenAI Chat Completions API (uses OPENAI_API_KEY)
//!   openai:gpt-4o          — same, larger model
//!   local:<model>          — Ollama at OLLAMA_URL (default http://127.0.0.1:11434)
//!
//! Constraints:
//!   - Rate-limited: max 10 outbound LLM calls per minute
//!   - Cached: 24h TTL keyed by session_id + first-prompt-sha
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
        *self.model.write().unwrap() = m;
        // Drop cache so a new model can re-name everything.
        self.cache.write().unwrap().clear();
    }

    /// Decide whether the rate limiter would allow another call.
    /// Side-effect-free.
    fn rate_allowed(&self) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        let calls = self.recent_calls.read().unwrap();
        let in_window = calls.iter().filter(|t| now.duration_since(**t) < window).count();
        in_window < RATE_LIMIT_MAX_CALLS
    }

    fn record_call(&self) {
        let now = Instant::now();
        let window = Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
        let mut calls = self.recent_calls.write().unwrap();
        calls.retain(|t| now.duration_since(*t) < window);
        calls.push(now);
    }

    fn cached(&self, session_id: &str, fp: &str) -> Option<Option<String>> {
        let cache = self.cache.read().unwrap();
        let entry = cache.get(session_id)?;
        if entry.prompt_fp != fp {
            return None;
        }
        if entry.built_at.elapsed().as_secs() > CACHE_TTL_SECS {
            return None;
        }
        Some(entry.name.clone())
    }

    fn store(&self, session_id: &str, name: Option<String>, fp: String) {
        let mut cache = self.cache.write().unwrap();
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
    let model_snapshot = state.model.read().unwrap().clone();
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
    let activity = extract_recent_activity(&content);
    if activity.is_empty() {
        return NameResponse {
            session_id: session_id.to_string(),
            name: None,
            source: "no-prompt".to_string(),
            cached: false,
        };
    }
    // Fingerprint includes the most-recent event timestamp so the
    // cache invalidates as new activity arrives — the prior shape
    // pinned only on first-prompt and never refreshed.
    let fp = fingerprint(&format!("{}|{}", activity.last_ts, activity.first_prompt));

    if let Some(cached) = state.cached(session_id, &fp) {
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

    let prompt_blob = format_activity_blob(&activity);
    let name = match llm::chat(
        model,
        llm::ChatRequest {
            system: SYSTEM_PROMPT,
            user: &prompt_blob,
            max_tokens: 60,
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

/// Hard cap on the activity window the LLM sees. The blob can be
/// huge for long sessions; the LLM only needs a representative slice.
const ACTIVITY_WINDOW_SECS: i64 = 3600;
const MAX_TOOL_PATTERNS: usize = 20;
const MAX_RECENT_PROMPTS: usize = 3;

/// Structured snapshot of what a session has been doing in its
/// most-recent activity window. Fed to the LLM for naming.
#[derive(Debug, Default, Clone)]
pub struct RecentActivity {
    /// First user prompt (always — anchors the session even if no
    /// activity in the last window). Empty when the transcript has
    /// no usable user message.
    pub first_prompt: String,
    /// User prompts from the activity window (most recent first).
    /// Truncated to a handful — these are the strongest steering
    /// signal for naming.
    pub recent_prompts: Vec<String>,
    /// Tool-call patterns in the window, descending by count.
    /// Each entry: (count, "ToolName: target summary").
    pub tool_patterns: Vec<(u32, String)>,
    /// ISO-8601 of the most-recent line we saw. Empty if none.
    /// Doubles as a cache-invalidation key so naming refreshes as
    /// new activity arrives.
    pub last_ts: String,
}

impl RecentActivity {
    pub fn is_empty(&self) -> bool {
        self.first_prompt.is_empty() && self.tool_patterns.is_empty()
    }
}

/// Walk the JSONL, collect first user prompt + recent activity window.
/// "Recent" means within ACTIVITY_WINDOW_SECS of the session's most
/// recent timestamp — i.e. relative to session time, not wall clock,
/// so paused/dormant sessions still produce sensible names.
fn extract_recent_activity(jsonl: &str) -> RecentActivity {
    let mut first_user = String::new();
    let mut entries: Vec<(String, String, serde_json::Value)> = Vec::new();
    // First pass: parse + collect (ts, type, value) for every line.
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(line) else { continue };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
        let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string();
        if typ == "user" && first_user.is_empty() {
            if let Some(text) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('<') && !trimmed.starts_with("Caveat:") {
                    first_user = trimmed.chars().take(800).collect();
                }
            }
        }
        entries.push((ts, typ, v));
    }

    // Determine the most-recent timestamp + the cutoff for "the last hour".
    let last_ts = entries
        .iter()
        .rev()
        .find_map(|(ts, _, _)| if ts.is_empty() { None } else { Some(ts.clone()) })
        .unwrap_or_default();
    let cutoff = parse_iso8601_secs(&last_ts).map(|t| t - ACTIVITY_WINDOW_SECS);

    // Second pass: collect tool calls + user prompts within the
    // window. Counts the same (name, summary) pair so dominant
    // patterns (e.g. "Edit on naming.rs ×12") surface.
    let mut tool_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut recent_prompts: Vec<String> = Vec::new();
    for (ts, typ, v) in &entries {
        // If we have a cutoff, skip anything older than it.
        if let (Some(cut), Some(ts_secs)) = (cutoff, parse_iso8601_secs(ts)) {
            if ts_secs < cut {
                continue;
            }
        }
        match typ.as_str() {
            "user" => {
                if let Some(text) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty()
                        && !trimmed.starts_with('<')
                        && !trimmed.starts_with("Caveat:")
                        && recent_prompts.len() < MAX_RECENT_PROMPTS * 4
                    {
                        recent_prompts.push(trimmed.chars().take(200).collect());
                    }
                }
            }
            "assistant" => {
                if let Some(blocks) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                            continue;
                        }
                        let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                        let inp = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
                        let summary = crate::activity::tool_summary(name, &inp);
                        let key = format!("{name}: {summary}");
                        *tool_counts.entry(key).or_insert(0) += 1;
                    }
                }
            }
            _ => {}
        }
    }

    // Sort tool patterns by count desc, truncate.
    let mut tool_patterns: Vec<(u32, String)> =
        tool_counts.into_iter().map(|(k, c)| (c, k)).collect();
    tool_patterns.sort_by(|a, b| b.0.cmp(&a.0));
    tool_patterns.truncate(MAX_TOOL_PATTERNS);

    // Recent prompts: take the LAST N (most recent within the window).
    recent_prompts.reverse();
    recent_prompts.truncate(MAX_RECENT_PROMPTS);

    RecentActivity {
        first_prompt: first_user,
        recent_prompts,
        tool_patterns,
        last_ts,
    }
}

/// Minimal ISO-8601 → epoch seconds. Returns None on parse failure.
/// We don't pull chrono for this — the activity window is forgiving
/// and we only need rough ordering. Format we see: "2026-05-27T18:33:12.345Z".
fn parse_iso8601_secs(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    // Strip fractional seconds and the trailing 'Z' if present so
    // the format matches what NaiveDateTime accepts.
    let trimmed = s
        .split_once('.')
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| s.trim_end_matches('Z').to_string());
    chrono::DateTime::parse_from_rfc3339(&format!("{trimmed}Z"))
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(&trimmed, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|n| n.and_utc().timestamp())
        })
}

fn fingerprint(prompt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Format the activity snapshot into a compact blob the LLM sees.
/// Order: dominant tool patterns first (the "what is this doing"
/// signal), then recent prompts (the "what is the operator asking for"
/// steering), then the first prompt (the long-term anchor).
fn format_activity_blob(a: &RecentActivity) -> String {
    let mut s = String::new();
    if !a.tool_patterns.is_empty() {
        s.push_str("Recent activity (last 1h, by count):\n");
        for (count, pattern) in &a.tool_patterns {
            use std::fmt::Write as _;
            let _ = writeln!(s, "  {count}× {pattern}");
        }
    }
    if !a.recent_prompts.is_empty() {
        s.push_str("\nRecent operator prompts:\n");
        for p in &a.recent_prompts {
            s.push_str("- ");
            s.push_str(p);
            s.push('\n');
        }
    }
    if !a.first_prompt.is_empty() {
        s.push_str("\nSession's first prompt:\n");
        s.push_str(&a.first_prompt);
    }
    s
}

const SYSTEM_PROMPT: &str = "You produce concise, terse, technical labels for what a Claude Code session is actively working on.

INPUT: a snapshot of the session's recent tool calls (with counts) and the operator's recent prompts.

OUTPUT RULES:
- ONE LINE. Up to ~8 words / 80 characters.
- Concrete and specific. Name the file/module/feature/bug when the data supports it.
- Prefer the DOMINANT activity in the last hour over the session's first prompt.
- No quotes, no trailing punctuation, no commentary.
- Lowercase. Use slashes, plus signs, or arrows for compound concepts.

EXAMPLES of good output:
- viz allowlist + vllm wiring
- naming.rs activity-window refactor
- sandbox-bootstrap gh + gitconfig
- auth migration rollback
- stuck-pinning reply-kind fix
- memory-server jwt scoping

EXAMPLES of bad output:
- 'working on viz' (too vague)
- 'The session is editing files' (commentary, not label)
- 'multi-file refactor' (no concrete subject)";

/// Sanitize the LLM output: trim quotes/punctuation, enforce a
/// reasonable upper bound, kill obvious refusals/fabrications.
/// The cap is char-based, not word-based — the new prompt produces
/// multi-word technical labels like "naming.rs activity-window
/// refactor" that the old 3-word truncation would have shredded.
pub fn sanitize_name(raw: &str) -> Option<String> {
    const MAX_CHARS: usize = 80;
    const MAX_WORDS: usize = 12;

    // First line only — the LLM should give one line; if it gives
    // more, the first is canonical and the rest is commentary.
    let first_line = raw.lines().next().unwrap_or("").trim();
    // Reject obvious fabrications BEFORE trimming so a fully-fenced
    // label like ```viz refactor``` doesn't slip through after the
    // trim strips its delimiters.
    let lower_raw = first_line.to_lowercase();
    if lower_raw.contains("i cannot")
        || lower_raw.contains("i'm sorry")
        || lower_raw.contains("i am sorry")
        || lower_raw.contains("```")
        || first_line.contains('[')
    {
        return None;
    }
    let cleaned: String = first_line
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == ',' || c == '`')
        .to_string();
    if cleaned.is_empty() {
        return None;
    }
    // Word and char caps, whichever is tighter.
    let words: Vec<&str> = cleaned.split_whitespace().take(MAX_WORDS).collect();
    if words.is_empty() {
        return None;
    }
    let mut out = words.join(" ").to_lowercase();
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect::<String>().trim_end().to_string();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_multi_word_technical_labels() {
        // The old 3-word cap shredded labels like this; the new
        // budget is 12 words / 80 chars.
        assert_eq!(
            sanitize_name("naming.rs activity-window refactor").as_deref(),
            Some("naming.rs activity-window refactor"),
        );
        assert_eq!(
            sanitize_name("viz allowlist + vllm wiring").as_deref(),
            Some("viz allowlist + vllm wiring"),
        );
    }

    #[test]
    fn sanitize_caps_at_twelve_words() {
        let long = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen";
        let out = sanitize_name(long).unwrap();
        assert_eq!(out.split_whitespace().count(), 12);
    }

    #[test]
    fn sanitize_caps_at_80_chars() {
        // 12 words of "longword" = 96 chars including spaces — char
        // cap kicks in before the word cap.
        let long = "longword ".repeat(12);
        let out = sanitize_name(long.trim()).unwrap();
        assert!(out.chars().count() <= 80, "got {} chars", out.chars().count());
    }

    #[test]
    fn sanitize_takes_only_first_line() {
        let multiline = "naming.rs refactor\nThis is commentary the LLM tacked on";
        assert_eq!(sanitize_name(multiline).as_deref(), Some("naming.rs refactor"));
    }

    #[test]
    fn sanitize_strips_quotes_and_punct() {
        assert_eq!(sanitize_name("\"Scoop watchdog.\"").as_deref(), Some("scoop watchdog"));
    }

    #[test]
    fn sanitize_rejects_refusals_and_fences() {
        assert!(sanitize_name("I cannot determine").is_none());
        assert!(sanitize_name("I'm sorry").is_none());
        assert!(sanitize_name("```viz refactor```").is_none());
        assert!(sanitize_name("[viz refactor]").is_none());
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
        assert!(s.model.read().unwrap().is_none());
    }

    #[test]
    fn extract_recent_activity_counts_tool_patterns() {
        // Construct a tiny JSONL with two Edit calls on the same file
        // and one Bash. The dominant pattern should rank first.
        let jsonl = r#"{"type":"user","timestamp":"2026-05-27T18:00:00Z","message":{"content":"start"}}
{"type":"assistant","timestamp":"2026-05-27T18:00:01Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/x/y.rs"}}]}}
{"type":"assistant","timestamp":"2026-05-27T18:00:02Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/x/y.rs"}}]}}
{"type":"assistant","timestamp":"2026-05-27T18:00:03Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}
"#;
        let a = extract_recent_activity(jsonl);
        assert!(!a.tool_patterns.is_empty());
        // First pattern should be the Edit (count 2), Bash second (count 1).
        assert_eq!(a.tool_patterns[0].0, 2);
        assert!(a.tool_patterns[0].1.starts_with("Edit"));
        assert_eq!(a.first_prompt, "start");
        assert_eq!(a.last_ts, "2026-05-27T18:00:03Z");
    }

    #[test]
    fn extract_recent_activity_respects_1h_window() {
        // A first prompt 3h before any tool calls should still be
        // captured as first_prompt (it's the long-term anchor) but
        // NOT counted as a tool pattern. Tool calls in the last hour
        // dominate.
        let jsonl = r#"{"type":"user","timestamp":"2026-05-27T15:00:00Z","message":{"content":"ancient prompt"}}
{"type":"assistant","timestamp":"2026-05-27T15:00:01Z","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/old.rs"}}]}}
{"type":"user","timestamp":"2026-05-27T18:30:00Z","message":{"content":"new prompt"}}
{"type":"assistant","timestamp":"2026-05-27T18:30:01Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/new.rs"}}]}}
"#;
        let a = extract_recent_activity(jsonl);
        assert_eq!(a.first_prompt, "ancient prompt");
        // Only the recent (within 1h of last_ts) Edit should be counted.
        assert_eq!(a.tool_patterns.len(), 1);
        assert!(a.tool_patterns[0].1.contains("Edit"));
        // Recent prompts list should include "new prompt".
        assert!(a.recent_prompts.iter().any(|p| p == "new prompt"));
        // And NOT include "ancient prompt" (outside the window).
        assert!(!a.recent_prompts.iter().any(|p| p == "ancient prompt"));
    }

    #[test]
    fn parse_iso8601_handles_fractional_and_z() {
        assert!(parse_iso8601_secs("2026-05-27T18:30:01.234Z").is_some());
        assert!(parse_iso8601_secs("2026-05-27T18:30:01Z").is_some());
        assert!(parse_iso8601_secs("").is_none());
        assert!(parse_iso8601_secs("not-a-date").is_none());
    }
}
