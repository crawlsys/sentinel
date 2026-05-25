//! Anthropic API Client
//!
//! Shared client for Sonnet 4.6, Opus 4.6, and Haiku 4.5.
//! Used by the skill classifier (skill router).
//!
//! Note: AI judge functionality has moved to `rig_judge.rs` (multi-model via Rig).

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use sentinel_domain::judge::JudgeModel;
use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Map domain `JudgeModel` tier to Anthropic API model ID.
///
/// This client talks directly to Anthropic and has no access to non-Anthropic
/// models — Kimi K2.6 maps to Sonnet 4.6 here as the closest review-tier
/// substitute. The canonical Kimi path is `OpenRouter` via `rig_judge.rs`;
/// this client is the fallback when `OPENROUTER_API_KEY` is unset.
const fn model_id(model: JudgeModel) -> &'static str {
    match model {
        JudgeModel::Codex => "claude-haiku-4-5-20251001",
        // No Kimi on Anthropic — map to closest review-tier model. Caller
        // should prefer OpenRouter (rig_judge) for actual Kimi K2.6 routing.
        JudgeModel::Kimi | JudgeModel::Sonnet => "claude-sonnet-4-6",
        JudgeModel::Opus => "claude-opus-4-6",
    }
}

/// Anthropic API client
#[derive(Clone)]
pub struct AnthropicClient {
    client: Client,
    api_key: String,
}

#[derive(Serialize)]
struct ApiRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ApiMessage>,
    system: Option<String>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ApiContent>,
}

#[derive(Deserialize)]
struct ApiContent {
    text: Option<String>,
}

impl AnthropicClient {
    /// Create a new client with API key
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
        }
    }

    /// Create from environment variable
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        Ok(Self::new(key))
    }

    /// Send a message to the API
    pub async fn message(
        &self,
        model: JudgeModel,
        system: &str,
        user_message: &str,
        max_tokens: u32,
    ) -> Result<String> {
        let request = ApiRequest {
            model: model_id(model).to_string(),
            max_tokens,
            messages: vec![ApiMessage {
                role: "user".to_string(),
                content: user_message.to_string(),
            }],
            system: Some(system.to_string()),
        };

        let response = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .context("Failed to send API request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error {status}: {body}");
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .context("Failed to parse API response")?;

        api_response
            .content
            .first()
            .and_then(|c| c.text.clone())
            .context("No text content in API response")
    }

    /// Classify a message into a skill
    pub async fn classify_skill(
        &self,
        message: &str,
        candidates: &[String],
    ) -> Result<Option<String>> {
        let system = "You are a skill classifier. Given a user message, determine which skill best matches.\n\
                      Respond with ONLY the skill name, or 'none' if no skill matches.\n\
                      Available skills (if any specific candidates provided, prefer those):";

        let candidates_str = if candidates.is_empty() {
            "No specific candidates — use your knowledge of common development skills.".to_string()
        } else {
            candidates.join(", ")
        };

        let user_msg = format!("Candidates: {candidates_str}\n\nUser message: {message}");

        let response = self
            .message(JudgeModel::Codex, system, &user_msg, 50)
            .await?;

        let skill = response.trim().to_lowercase();
        if skill == "none" || skill.is_empty() {
            Ok(None)
        } else {
            Ok(Some(skill))
        }
    }
}

/// Implement the application layer's `AiClassifier` trait
#[async_trait::async_trait]
impl sentinel_application::classifier::AiClassifier for AnthropicClient {
    async fn classify(&self, message: &str, candidates: &[String]) -> Result<Option<String>> {
        self.classify_skill(message, candidates).await
    }
}

/// Map the domain `LlmModel` to the closest `JudgeModel` so we can reuse
/// the existing `model_id()` mapping.
const fn llm_to_judge(model: LlmModel) -> JudgeModel {
    match model {
        LlmModel::Haiku => JudgeModel::Codex,
        LlmModel::Sonnet => JudgeModel::Sonnet,
        LlmModel::Opus => JudgeModel::Opus,
    }
}

/// Implement the domain `LlmPort` so hooks can call any Anthropic model
/// without holding a concrete `AnthropicClient`. System prompt is empty —
/// callers embed any system context in the user prompt itself, which keeps
/// the port surface minimal.
#[async_trait::async_trait]
impl LlmPort for AnthropicClient {
    async fn complete(&self, request: LlmRequest) -> Result<String> {
        self.message(
            llm_to_judge(request.model),
            "",
            &request.prompt,
            request.max_tokens,
        )
        .await
    }
}
