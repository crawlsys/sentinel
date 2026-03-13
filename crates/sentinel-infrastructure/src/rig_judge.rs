//! Multi-model AI Judge via Rig LLM framework
//!
//! Uses Rig's provider system for Anthropic, OpenAI, and Cerebras
//! (via OpenAI-compatible `base_url`). Follows the same type-erased
//! adapter pattern as Vulcan's `RigAdapter` but tailored for `JudgeService`.

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::{anthropic, openai};
use std::sync::Arc;
use tracing::{debug, info};

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

const CEREBRAS_BASE_URL: &str = "https://api.cerebras.ai/v1";
const CEREBRAS_MODEL: &str = "zai-glm-4.7";
const OPENAI_MODEL: &str = "gpt-5.3-codex";
const ANTHROPIC_MODEL: &str = "claude-opus-4-6";

/// Type-erased prompt function: (system, user_msg) -> response text
type PromptFn =
    Arc<dyn Fn(String, String) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// A single Rig-backed judge provider
struct RigProvider {
    prompt_fn: PromptFn,
    name: &'static str,
}

impl RigProvider {
    /// Cerebras — OpenAI-compatible with custom `base_url`
    fn cerebras() -> Result<Self> {
        let key =
            std::env::var("CEREBRAS_API_KEY").context("CEREBRAS_API_KEY not set")?;
        let client: openai::CompletionsClient = openai::CompletionsClient::builder()
            .api_key(key)
            .base_url(CEREBRAS_BASE_URL)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build Cerebras client: {e}"))?;
        let client = Arc::new(client);
        Ok(Self {
            prompt_fn: Arc::new(move |system, user_msg| {
                let client = client.clone();
                Box::pin(async move {
                    let agent = AgentBuilder::new(client.completion_model(CEREBRAS_MODEL))
                        .preamble(&system)
                        .build();
                    let result: Result<String, _> = agent.prompt(user_msg).await;
                    result.map_err(|e| anyhow::anyhow!("Cerebras judge: {e}"))
                })
            }),
            name: "cerebras",
        })
    }

    /// OpenAI — standard endpoint
    fn openai() -> Result<Self> {
        let key =
            std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
        let client: openai::CompletionsClient = openai::CompletionsClient::builder()
            .api_key(key)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build OpenAI client: {e}"))?;
        let client = Arc::new(client);
        Ok(Self {
            prompt_fn: Arc::new(move |system, user_msg| {
                let client = client.clone();
                Box::pin(async move {
                    let agent = AgentBuilder::new(client.completion_model(OPENAI_MODEL))
                        .preamble(&system)
                        .build();
                    let result: Result<String, _> = agent.prompt(user_msg).await;
                    result.map_err(|e| anyhow::anyhow!("OpenAI judge: {e}"))
                })
            }),
            name: "openai",
        })
    }

    /// Anthropic — native provider
    fn anthropic() -> Result<Self> {
        let key =
            std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        let client: anthropic::Client = anthropic::Client::builder()
            .api_key(key)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build Anthropic client: {e}"))?;
        let client = Arc::new(client);
        Ok(Self {
            prompt_fn: Arc::new(move |system, user_msg| {
                let client = client.clone();
                Box::pin(async move {
                    let agent =
                        AgentBuilder::new(client.completion_model(ANTHROPIC_MODEL))
                            .preamble(&system)
                            .build();
                    let result: Result<String, _> = agent.prompt(user_msg).await;
                    result.map_err(|e| anyhow::anyhow!("Anthropic judge: {e}"))
                })
            }),
            name: "anthropic",
        })
    }

    /// Send a judge prompt and parse the JSON verdict
    async fn judge(&self, system: &str, user_msg: &str) -> Result<JudgeVerdict> {
        let text =
            (self.prompt_fn)(system.to_string(), user_msg.to_string()).await?;
        debug!(
            provider = self.name,
            response_len = text.len(),
            "Judge response received"
        );

        serde_json::from_str::<JudgeVerdict>(&text)
            .or_else(|_| extract_json_from_markdown(&text))
            .map(|v| v.sanitized()) // Attack #127: clamp confidence to [0.0, 1.0]
            .context("Failed to parse judge verdict JSON")
    }
}

/// Multi-model judge — routes to different AI providers by criticality.
///
/// Initialization is best-effort per provider: if a key is missing the
/// provider is `None` and the fallback chain tries the next one.
pub struct MultiModelJudge {
    cerebras: Option<RigProvider>,
    openai: Option<RigProvider>,
    anthropic: Option<RigProvider>,
}

impl MultiModelJudge {
    /// Initialize from environment variables.
    pub fn from_env() -> Self {
        let cerebras = RigProvider::cerebras().ok();
        let openai = RigProvider::openai().ok();
        let anthropic = RigProvider::anthropic().ok();

        let providers: Vec<&str> = [
            cerebras.as_ref().map(|_| "cerebras"),
            openai.as_ref().map(|_| "openai"),
            anthropic.as_ref().map(|_| "anthropic"),
        ]
        .into_iter()
        .flatten()
        .collect();

        // **Attack #125 fix**: Warn loudly if no AI providers are available.
        // Without any provider, all proof submissions will fail. This is fail-safe
        // (phases can't be proven → tools stay blocked), but the user should know.
        if providers.is_empty() {
            eprintln!(
                "[sentinel] WARNING: No AI judge providers available. \
                 Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or CEREBRAS_API_KEY. \
                 All proof submissions will fail until a provider is configured."
            );
        }
        info!(providers = ?providers, "MultiModelJudge initialized via Rig");

        Self {
            cerebras,
            openai,
            anthropic,
        }
    }

    /// Returns `true` if at least one provider is available.
    pub fn has_any_provider(&self) -> bool {
        self.cerebras.is_some()
            || self.openai.is_some()
            || self.anthropic.is_some()
    }
}

#[async_trait::async_trait]
impl sentinel_application::judge_service::JudgeService for MultiModelJudge {
    async fn evaluate(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        // Route by model criticality — Opus never falls back to weaker tiers
        let provider = match model {
            JudgeModel::Haiku => self
                .cerebras
                .as_ref()
                .or(self.openai.as_ref())
                .or(self.anthropic.as_ref()),
            JudgeModel::Sonnet => self
                .openai
                .as_ref()
                .or(self.anthropic.as_ref()),
            JudgeModel::Opus => self
                .anthropic
                .as_ref(),
        };

        let provider = provider.ok_or_else(|| {
            anyhow::anyhow!(
                "No AI judge available for {model} tier — required provider not configured"
            )
        })?;

        info!(
            provider = provider.name,
            model = %model,
            skill = skill,
            phase = phase_id,
            "Routing judge evaluation"
        );

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

        provider.judge(&system, &user_msg).await
    }
}

/// Extract the first balanced JSON object from text.
///
/// Walks forward from the first `{`, tracking brace depth, and extracts
/// the first complete top-level object. This avoids the naive first-`{`
/// to last-`}` approach which can span unrelated braces.
fn extract_json_from_markdown(
    text: &str,
) -> Result<JudgeVerdict, serde_json::Error> {
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i;
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escape_next = false;

            for j in start..bytes.len() {
                if escape_next {
                    escape_next = false;
                    continue;
                }
                match bytes[j] {
                    b'\\' if in_string => escape_next = true,
                    b'"' => in_string = !in_string,
                    b'{' if !in_string => depth += 1,
                    b'}' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            let candidate = &text[start..=j];
                            if let Ok(v) = serde_json::from_str::<JudgeVerdict>(candidate) {
                                return Ok(v.sanitized()); // Attack #127: clamp confidence
                            }
                            // Not a valid verdict — keep scanning
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i = start + 1;
        } else {
            i += 1;
        }
    }

    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_direct() {
        let json = r#"{"sufficient": true, "confidence": 0.95, "reasoning": "All tests passed"}"#;
        let verdict = extract_json_from_markdown(json).unwrap();
        assert!(verdict.sufficient);
        assert!(verdict.confidence > 0.9);
    }

    #[test]
    fn test_extract_json_from_code_block() {
        let text = "Here is my analysis:\n```json\n{\"sufficient\": false, \"confidence\": 0.3, \"reasoning\": \"Missing tests\", \"requested_evidence\": [\"unit tests\"]}\n```";
        let verdict = extract_json_from_markdown(text).unwrap();
        assert!(!verdict.sufficient);
        assert_eq!(verdict.requested_evidence.unwrap().len(), 1);
    }

    #[test]
    fn test_extract_json_with_surrounding_text() {
        let text = "The evidence is insufficient. {\"sufficient\": false, \"confidence\": 0.2, \"reasoning\": \"No proof\"} That is my verdict.";
        let verdict = extract_json_from_markdown(text).unwrap();
        assert!(!verdict.sufficient);
    }

    #[test]
    fn test_multi_model_judge_no_keys() {
        // With no env vars set, all providers should be None
        let judge = MultiModelJudge {
            cerebras: None,
            openai: None,
            anthropic: None,
        };
        assert!(!judge.has_any_provider());
    }
}
