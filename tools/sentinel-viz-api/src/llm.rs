//! Shared LLM client. Used by `naming.rs` (3-word session names) and
//! `summary.rs` (card/wait/narrative summaries). One env var controls
//! the model: `SENTINEL_VIZ_NAMING_MODEL` keeps that legacy name for
//! continuity but is renamed in spirit — it picks the LLM for ALL
//! viz AI features.
//!
//! WORKSTREAM: sentinel-viz — internal to this crate. The outbound
//! HTTP call to OpenAI / Ollama is a third-party API, not a Sentinel
//! boundary.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum ModelConfig {
    OpenAi { model: String, api_key: String },
    /// OpenRouter is OpenAI-compatible — same request/response
    /// shape, different base URL. Multi-vendor access through one
    /// key. Default for the entire viz instance when
    /// OPENROUTER_API_KEY is set and no other model is named.
    OpenRouter { model: String, api_key: String },
    LocalOllama { model: String, base_url: String },
    /// Self-hosted vLLM (OpenAI-compatible /v1/chat/completions).
    /// Used for high-param local summarization models (e.g.
    /// Llama-3.1-405B-FP8 / Qwen2.5-72B-AWQ on the nighttime box).
    /// VLLM_BASE_URL points at the host serving /v1; API key is
    /// optional and defaults to a sentinel placeholder since most
    /// self-hosted vLLM instances run with auth disabled.
    Vllm { model: String, base_url: String, api_key: String },
}

/// Default base URL when SENTINEL_VIZ_NAMING_MODEL=vllm:<model> is
/// set but no VLLM_BASE_URL is. The operator's own host is the
/// most likely target so we don't fabricate a public default.
const DEFAULT_VLLM_BASE_URL: &str = "http://127.0.0.1:8000/v1";

/// Default model when OPENROUTER_API_KEY is set but no
/// SENTINEL_VIZ_NAMING_MODEL is. Cheap + good at summaries —
/// operator-friendly default for an "always-on" AI helper layer.
const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-4o-mini";

/// Operator-convention path on disk. If env doesn't expose a key,
/// fall back to this file (kept at mode 0600 by convention).
const OPENROUTER_KEY_PATH: &str = ".config/openrouter/api_key";

/// Public wrapper for callers outside this module (server.rs's
/// set_config). Same behavior — looks at the operator-convention
/// path `~/.config/openrouter/api_key`.
pub fn load_openrouter_key_from_disk_public() -> Option<String> {
    load_openrouter_key_from_disk()
}

fn load_openrouter_key_from_disk() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home).join(OPENROUTER_KEY_PATH);
    let contents = std::fs::read_to_string(&path).ok()?;
    let trimmed = contents.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        tracing::info!(?path, "loaded OPENROUTER_API_KEY from disk fallback");
        Some(trimmed)
    }
}

impl ModelConfig {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("SENTINEL_VIZ_NAMING_MODEL").ok();
        let explicit = match raw.as_deref() {
            None | Some("") | Some("none") => None,
            Some(s) if s.starts_with("openai:") => {
                let m = s.trim_start_matches("openai:").to_string();
                match std::env::var("OPENAI_API_KEY") {
                    Ok(k) if !k.is_empty() => Some(Self::OpenAi { model: m, api_key: k }),
                    _ => {
                        tracing::warn!("openai:{m} requires OPENAI_API_KEY; disabling LLM features");
                        return None;
                    }
                }
            }
            Some(s) if s.starts_with("openrouter:") => {
                let m = s.trim_start_matches("openrouter:").to_string();
                match std::env::var("OPENROUTER_API_KEY") {
                    Ok(k) if !k.is_empty() => Some(Self::OpenRouter { model: m, api_key: k }),
                    _ => {
                        tracing::warn!("openrouter:{m} requires OPENROUTER_API_KEY; disabling LLM features");
                        return None;
                    }
                }
            }
            Some(s) if s.starts_with("local:") => {
                let m = s.trim_start_matches("local:").to_string();
                let base = std::env::var("OLLAMA_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
                Some(Self::LocalOllama { model: m, base_url: base })
            }
            Some(s) if s.starts_with("vllm:") => {
                let m = s.trim_start_matches("vllm:").to_string();
                let base = std::env::var("VLLM_BASE_URL")
                    .unwrap_or_else(|_| DEFAULT_VLLM_BASE_URL.to_string());
                // vLLM accepts any non-empty Bearer when --api-key
                // isn't set on the server. Use VLLM_API_KEY if the
                // operator pinned one, else a placeholder.
                let key = std::env::var("VLLM_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| "sentinel-viz".to_string());
                Some(Self::Vllm { model: m, base_url: base, api_key: key })
            }
            Some(other) => {
                tracing::warn!("unknown SENTINEL_VIZ_NAMING_MODEL '{other}'; disabling LLM features");
                return None;
            }
        };

        if let Some(cfg) = explicit {
            return Some(cfg);
        }

        // Default for the entire instance: if the operator has set
        // OPENROUTER_API_KEY OR has the key on disk at
        // ~/.config/openrouter/api_key (operator convention),
        // wire up a cheap+good summary model. The on-disk path
        // means the API picks the right key across reboots /
        // launch contexts without needing the env to be in scope.
        let key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(load_openrouter_key_from_disk);
        if let Some(k) = key {
            let m = std::env::var("SENTINEL_VIZ_NAMING_MODEL_DEFAULT")
                .unwrap_or_else(|_| DEFAULT_OPENROUTER_MODEL.to_string());
            tracing::info!(
                "no explicit SENTINEL_VIZ_NAMING_MODEL set; defaulting to openrouter:{m}"
            );
            return Some(Self::OpenRouter { model: m, api_key: k });
        }
        None
    }

    pub fn label(&self) -> String {
        match self {
            Self::OpenAi { model, .. } => format!("openai:{model}"),
            Self::OpenRouter { model, .. } => format!("openrouter:{model}"),
            Self::LocalOllama { model, .. } => format!("local:{model}"),
            Self::Vllm { model, .. } => format!("vllm:{model}"),
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
        ModelConfig::OpenAi { model, api_key } => {
            openai_compatible("https://api.openai.com/v1/chat/completions", model, api_key, &req, None).await
        }
        ModelConfig::OpenRouter { model, api_key } => {
            // OpenRouter recommends sending HTTP-Referer + X-Title
            // headers for attribution; sending the viz identifier
            // helps the operator track which app burned the credit.
            openai_compatible(
                "https://openrouter.ai/api/v1/chat/completions",
                model,
                api_key,
                &req,
                Some([
                    ("HTTP-Referer", "https://github.com/kvncrw/sentinel-1"),
                    ("X-Title", "sentinel-viz"),
                ]
                .as_ref()),
            )
            .await
        }
        ModelConfig::LocalOllama { model, base_url } => ollama(model, base_url, &req).await,
        ModelConfig::Vllm { model, base_url, api_key } => {
            // vLLM speaks OpenAI's chat/completions wire format
            // verbatim. No attribution headers needed (self-hosted).
            // base_url already includes /v1 by operator convention.
            let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            openai_compatible(&url, model, api_key, &req, None).await
        }
    }
}

/// Shared OpenAI-compatible chat completions caller. OpenRouter
/// piggy-backs on this with an extra `extra_headers` set for
/// attribution per OpenRouter's docs.
async fn openai_compatible(
    url: &str,
    model: &str,
    api_key: &str,
    req: &ChatRequest<'_>,
    extra_headers: Option<&[(&'static str, &'static str)]>,
) -> Result<String> {
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
    let mut request = client.post(url).bearer_auth(api_key).json(&body);
    if let Some(hs) = extra_headers {
        for (k, v) in hs {
            request = request.header(*k, *v);
        }
    }
    let resp: Resp = request.send().await?.error_for_status()?.json().await?;
    Ok(resp
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default())
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
