//! House LLM HTTP client — a thin, dependency-light chat client for the
//! OpenAI-compatible `/chat/completions` API.
//!
//! Sentinel does a lot of LLM work (adversarial judges, worker-model scorers,
//! classifiers, OpenRouter + Ollama routing) and every one of those paths needs
//! exactly the same thing: send a system preamble + a user message, get a string
//! back. This module owns that call directly over the workspace `reqwest`
//! (0.12, ring-based TLS) instead of pulling in a full LLM-orchestration
//! framework.
//!
//! It intentionally supports ONLY the request shape sentinel uses:
//! `POST {base_url}/chat/completions` with `{model, messages, max_tokens?,
//! temperature?}` and Bearer auth, parsing `choices[0].message.content`. No
//! tools, streaming, embeddings, RAG, or multi-turn — add those only if a real
//! call site needs them.
//!
//! Providers are just a base URL + key:
//! - **OpenRouter** — `https://openrouter.ai/api/v1` ([`ChatClient::openrouter`]).
//! - **OpenAI-compatible** (Ollama local/cloud, any drop-in endpoint) —
//!   [`ChatClient::openai_compat`] with an explicit base URL.
//!
//! Gzip is handled by the workspace `reqwest` (the `gzip` feature): OpenRouter
//! gzip-compresses responses and a client that advertises `Accept-Encoding:
//! gzip` without decoding fails the body read ("error decoding response body")
//! on some models — enabling the feature makes decoding automatic.

use anyhow::{Context, Result};
use serde::Deserialize;

/// OpenRouter's OpenAI-compatible API base.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// A thin OpenAI-compatible chat client (one provider endpoint + key).
///
/// Cheap to clone — it wraps a `reqwest::Client` (which is an `Arc` internally)
/// plus the base URL and key. Build one and reuse it across many `complete`
/// calls (e.g. behind a `PromptFn`).
#[derive(Clone)]
pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ChatClient {
    /// Client for OpenRouter (`https://openrouter.ai/api/v1`). `key` is the
    /// `OPENROUTER_API_KEY` value.
    pub fn openrouter(key: impl Into<String>) -> Result<Self> {
        Self::openai_compat(OPENROUTER_BASE_URL, key)
    }

    /// Client for any OpenAI-compatible endpoint at `base_url` (e.g. Ollama
    /// local `http://localhost:11434/v1`, Ollama cloud `https://ollama.com/v1`).
    /// The trailing `/chat/completions` is appended by [`Self::complete`], so
    /// `base_url` should be the API root WITHOUT it (a trailing slash is fine).
    pub fn openai_compat(base_url: impl Into<String>, key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .context("failed to build reqwest client for LLM chat")?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: key.into(),
        })
    }

    /// Send a single chat completion and return the assistant text.
    ///
    /// - `system` — optional system preamble; omitted from `messages` when
    ///   `None` (some sentinel call sites send no system message).
    /// - `max_tokens` / `temperature` — added to the body only when `Some`.
    ///
    /// Returns the content of the first choice, or an error carrying the HTTP
    /// status + a snippet of the response body when the call fails or the
    /// response shape is unexpected.
    pub async fn complete(
        &self,
        model: &str,
        system: Option<&str>,
        user: &str,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> Result<String> {
        let mut messages: Vec<Message<'_>> = Vec::with_capacity(2);
        if let Some(sys) = system {
            messages.push(Message { role: "system", content: sys });
        }
        messages.push(Message { role: "user", content: user });

        let mut body = serde_json::json!({ "model": model, "messages": messages });
        if let Some(mt) = max_tokens {
            body["max_tokens"] = serde_json::json!(mt);
        }
        if let Some(t) = temperature {
            body["temperature"] = serde_json::json!(t);
        }

        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("chat request to {url} failed to send"))?;

        let status = resp.status();
        if !status.is_success() {
            let snippet = resp.text().await.unwrap_or_default();
            let snippet = truncate(&snippet, 500);
            anyhow::bail!("chat request to {url} returned {status}: {snippet}");
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .with_context(|| format!("failed to decode chat response from {url}"))?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .context("chat response had no choices")
    }
}

/// One chat message in the request body.
#[derive(serde::Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// The subset of the `/chat/completions` response we read.
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    /// Absent/null content (e.g. a pure tool-call response) decodes to empty.
    #[serde(default)]
    content: String,
}

/// Truncate a string to `max` bytes on a char boundary, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let c = ChatClient::openai_compat("http://localhost:11434/v1/", "k").unwrap();
        assert_eq!(c.base_url, "http://localhost:11434/v1");
        let c2 = ChatClient::openrouter("k").unwrap();
        assert_eq!(c2.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn request_body_omits_system_when_none_and_includes_opts() {
        // system=None → only the user message; max_tokens/temperature included.
        let mut msgs: Vec<Message<'_>> = Vec::new();
        // mirror complete()'s body assembly
        let system: Option<&str> = None;
        if let Some(sys) = system {
            msgs.push(Message { role: "system", content: sys });
        }
        msgs.push(Message { role: "user", content: "hi" });
        let mut body = serde_json::json!({ "model": "m", "messages": msgs });
        body["max_tokens"] = serde_json::json!(16u32);
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "no system message when None");
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(body["max_tokens"], 16);
    }

    #[test]
    fn request_body_includes_system_when_some() {
        let mut msgs: Vec<Message<'_>> = Vec::new();
        let system: Option<&str> = Some("you are a judge");
        if let Some(sys) = system {
            msgs.push(Message { role: "system", content: sys });
        }
        msgs.push(Message { role: "user", content: "verdict?" });
        let body = serde_json::json!({ "model": "m", "messages": msgs });
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "you are a judge");
        assert_eq!(arr[1]["role"], "user");
    }

    #[test]
    fn parses_choices_content() {
        let raw = serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": "the answer" } }]
        });
        let parsed: ChatResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.choices[0].message.content, "the answer");
    }

    #[test]
    fn parses_missing_content_as_empty() {
        let raw = serde_json::json!({ "choices": [{ "message": { "role": "assistant" } }] });
        let parsed: ChatResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.choices[0].message.content, "");
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let s = "│".repeat(400); // 3 bytes each = 1200 bytes
        let t = truncate(&s, 500);
        assert!(t.ends_with('…'));
        assert!(t.len() <= 504); // 500ish + ellipsis bytes, never mid-char-panic
    }
}
