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
    LocalOllama { model: String, base_url: String },
}

impl ModelConfig {
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("SENTINEL_VIZ_NAMING_MODEL").ok();
        match raw.as_deref() {
            None | Some("") | Some("none") => None,
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
