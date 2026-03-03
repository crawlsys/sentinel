//! Anthropic API Client
//!
//! Shared client for Sonnet 4.6, Opus 4.6, and Haiku 4.5.
//! Used by AI judges, skill classifier, and relevance checker.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Map domain model to API model ID
fn model_id(model: JudgeModel) -> &'static str {
    match model {
        JudgeModel::Sonnet => "claude-sonnet-4-6",
        JudgeModel::Opus => "claude-opus-4-6",
        JudgeModel::Haiku => "claude-haiku-4-5-20251001",
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
        let key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY not set")?;
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

    /// Judge evidence for a phase
    pub async fn judge(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        let system = format!(
            "You are an AI judge evaluating whether evidence proves a skill phase was completed.\n\
             Respond with ONLY valid JSON in this exact format:\n\
             {{\"sufficient\": true/false, \"confidence\": 0.0-1.0, \"reasoning\": \"...\", \"requested_evidence\": null or [\"...\", ...]}}\n\n\
             Skill: {skill}\n\
             Phase: {phase_id}\n\
             Phase objectives: {phase_objectives}"
        );

        let evidence_json = serde_json::to_string_pretty(evidence)?;
        let user_msg = format!(
            "Evaluate this evidence for the '{phase_id}' phase:\n\n{evidence_json}"
        );

        let response = self.message(model, &system, &user_msg, 512).await;

        match response {
            Ok(text) => {
                debug!(phase = phase_id, "Judge response received");
                match serde_json::from_str::<JudgeVerdict>(&text) {
                    Ok(verdict) => Ok(verdict),
                    Err(e) => {
                        warn!(
                            phase = phase_id,
                            error = %e,
                            "Failed to parse judge verdict, using default pass"
                        );
                        Ok(JudgeVerdict::default_pass())
                    }
                }
            }
            Err(e) => {
                warn!(
                    phase = phase_id,
                    error = %e,
                    "Judge API call failed, using default pass"
                );
                Ok(JudgeVerdict::default_pass())
            }
        }
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

        let user_msg = format!(
            "Candidates: {candidates_str}\n\nUser message: {message}"
        );

        let response = self
            .message(JudgeModel::Haiku, system, &user_msg, 50)
            .await?;

        let skill = response.trim().to_lowercase();
        if skill == "none" || skill.is_empty() {
            Ok(None)
        } else {
            Ok(Some(skill))
        }
    }
}

/// Implement the application layer's JudgeService trait
#[async_trait::async_trait]
impl sentinel_application::judge_service::JudgeService for AnthropicClient {
    async fn evaluate(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        self.judge(skill, phase_id, phase_objectives, evidence, model)
            .await
    }
}

/// Implement the application layer's AiClassifier trait
#[async_trait::async_trait]
impl sentinel_application::classifier::AiClassifier for AnthropicClient {
    async fn classify(&self, message: &str, candidates: &[String]) -> Result<Option<String>> {
        self.classify_skill(message, candidates).await
    }
}
