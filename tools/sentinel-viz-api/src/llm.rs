//! Shared LLM client. Used by `naming.rs` (3-word session names) and
//! `summary.rs` (card/wait/narrative summaries). One env var controls
//! the model: `SENTINEL_VIZ_NAMING_MODEL` keeps that legacy name for
//! continuity but is renamed in spirit — it picks the LLM for ALL
//! viz AI features.
//!
//! WORKSTREAM: sentinel-viz — internal to this crate. The outbound
//! HTTP call to `OpenAI` / Ollama is a third-party API, not a Sentinel
//! boundary.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum ModelConfig {
    OpenAi { model: String, api_key: String },
    LocalOllama { model: String, base_url: String },
}

impl ModelConfig {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("SENTINEL_VIZ_NAMING_MODEL").ok();
        match raw.as_deref() {
            None | Some("" | "none") => None,
            Some(s) if s.starts_with("openai:") => {
                let m = s.trim_start_matches("openai:").to_string();
                match std::env::var("OPENAI_API_KEY") {
                    Ok(k) if !k.is_empty() => Some(Self::OpenAi { model: m, api_key: k }),
                    _ => {
                        tracing::warn!("openai:{m} requires OPENAI_API_KEY; disabling LLM features");
                        None
                    }
                }
            }
            Some(s) if s.starts_with("local:") => {
                let m = s.trim_start_matches("local:").to_string();
                let base = std::env::var("OLLAMA_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
                // SSRF guard: refuse a non-loopback Ollama endpoint from
                // the env fallback unless the operator opted in via
                // SENTINEL_VIZ_OLLAMA_ALLOWLIST. Mirrors the runtime
                // check applied to /api/config writes.
                if let Err(e) = validate_ollama_url(&base) {
                    tracing::warn!("OLLAMA_URL rejected ({e}); disabling LLM features");
                    return None;
                }
                Some(Self::LocalOllama { model: m, base_url: base })
            }
            Some(other) => {
                tracing::warn!("unknown SENTINEL_VIZ_NAMING_MODEL '{other}'; disabling LLM features");
                None
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::OpenAi { model, .. } => format!("openai:{model}"),
            Self::LocalOllama { model, .. } => format!("local:{model}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    pub system: &'a str,
    pub user: &'a str,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout_secs: u64,
}

pub async fn chat(model: &ModelConfig, req: ChatRequest<'_>) -> Result<String> {
    match model {
        ModelConfig::OpenAi { model, api_key } => openai(model, api_key, &req).await,
        ModelConfig::LocalOllama { model, base_url } => ollama(model, base_url, &req).await,
    }
}

async fn openai(model: &str, api_key: &str, req: &ChatRequest<'_>) -> Result<String> {
    #[derive(Serialize)]
    struct Msg<'a> {
        role: &'a str,
        content: &'a str,
    }
    #[derive(Serialize)]
    struct Body<'a> {
        model: &'a str,
        messages: Vec<Msg<'a>>,
        max_tokens: u32,
        temperature: f32,
    }
    #[derive(Deserialize)]
    struct Resp {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: RespMsg,
    }
    #[derive(Deserialize)]
    struct RespMsg {
        content: String,
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(req.timeout_secs))
        .build()?;
    let body = Body {
        model,
        messages: vec![
            Msg { role: "system", content: req.system },
            Msg { role: "user", content: req.user },
        ],
        max_tokens: req.max_tokens,
        temperature: req.temperature,
    };
    let resp: Resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default())
}

/// SSRF guard for the Ollama base URL. The viz server POSTs to
/// `{base_url}/api/generate`, and the URL is operator-supplied (via
/// `/api/config` or the `OLLAMA_URL` env). Restrict it to a loopback
/// host so a malicious or mistaken value can't make the server probe
/// internal services (cloud metadata endpoints, intranet hosts, …).
///
/// Non-loopback hosts are permitted only when explicitly listed in
/// `SENTINEL_VIZ_OLLAMA_ALLOWLIST` (comma-separated host names, matched
/// case-insensitively against the URL host).
pub fn validate_ollama_url(raw: &str) -> Result<(), String> {
    let url = url::Url::parse(raw).map_err(|e| format!("invalid ollama_url '{raw}': {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("ollama_url scheme must be http or https, got '{other}'")),
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("ollama_url '{raw}' has no host"))?;
    if host_is_loopback(host) {
        return Ok(());
    }
    if ollama_host_allowlisted(host) {
        return Ok(());
    }
    Err(format!(
        "ollama_url host '{host}' is not loopback; set SENTINEL_VIZ_OLLAMA_ALLOWLIST to permit it"
    ))
}

/// `true` when `host` resolves to a loopback identity without a DNS
/// lookup: the literal `localhost`, or any IP literal whose address is
/// in a loopback range (`127.0.0.0/8`, `::1`).
fn host_is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // `Url::host_str` keeps the brackets on IPv6 literals (`[::1]`);
    // strip them before parsing so both families go through `IpAddr`.
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    unbracketed
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

fn ollama_host_allowlisted(host: &str) -> bool {
    let Ok(list) = std::env::var("SENTINEL_VIZ_OLLAMA_ALLOWLIST") else {
        return false;
    };
    list.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

async fn ollama(model: &str, base_url: &str, req: &ChatRequest<'_>) -> Result<String> {
    #[derive(Serialize)]
    struct Body<'a> {
        model: &'a str,
        prompt: String,
        stream: bool,
        options: Opts,
    }
    #[derive(Serialize)]
    struct Opts {
        temperature: f32,
        num_predict: u32,
    }
    #[derive(Deserialize)]
    struct Resp {
        response: String,
    }

    let prompt = format!("{}\n\n{}", req.system, req.user);
    let body = Body {
        model,
        prompt,
        stream: false,
        options: Opts {
            temperature: req.temperature,
            num_predict: req.max_tokens,
        },
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(req.timeout_secs))
        .build()?;
    let resp: Resp = client
        .post(format!("{base_url}/api/generate"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.response)
}

#[cfg(test)]
#[allow(unsafe_code)] // env mutation in tests; see allowlist_permits_named_external_host
mod tests {
    use super::*;

    #[test]
    fn loopback_urls_are_allowed() {
        for u in [
            "http://127.0.0.1:11434",
            "http://localhost:11434",
            "http://localhost",
            "https://127.0.0.1",
            "http://[::1]:11434",
            "http://127.0.0.5:11434", // anywhere in 127.0.0.0/8
        ] {
            assert!(validate_ollama_url(u).is_ok(), "should allow loopback: {u}");
        }
    }

    #[test]
    fn external_and_malformed_urls_are_rejected() {
        // External hosts (incl. the cloud metadata IP) are blocked
        // unless explicitly allowlisted, which this test does not set.
        for u in [
            "http://evil.com/api",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.5:11434",
            "http://example.internal",
        ] {
            assert!(validate_ollama_url(u).is_err(), "should reject external: {u}");
        }
        // Non-http(s) schemes are rejected even when loopback.
        assert!(validate_ollama_url("file:///etc/passwd").is_err());
        assert!(validate_ollama_url("ftp://127.0.0.1").is_err());
        // Garbage is rejected.
        assert!(validate_ollama_url("not a url").is_err());
        assert!(validate_ollama_url("http://").is_err());
    }

    #[test]
    fn allowlist_permits_named_external_host() {
        // SAFETY: env mutation is process-global. All env-sensitive
        // assertions are confined to this single test so they run
        // sequentially; the rejection tests above never set this var.
        unsafe {
            std::env::set_var("SENTINEL_VIZ_OLLAMA_ALLOWLIST", "ollama.internal, gpu-box");
        }
        assert!(validate_ollama_url("http://ollama.internal:11434").is_ok());
        assert!(validate_ollama_url("http://GPU-BOX:11434").is_ok()); // case-insensitive
        assert!(
            validate_ollama_url("http://other.host:11434").is_err(),
            "hosts not on the allowlist stay rejected"
        );
        unsafe {
            std::env::remove_var("SENTINEL_VIZ_OLLAMA_ALLOWLIST");
        }
    }
}
