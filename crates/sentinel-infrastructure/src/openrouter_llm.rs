//! OpenRouter-backed `LlmPort` — the standardized LLM path for hooks.
//!
//! Every hook that needs an LLM completion (`ctx.llm`) goes through this
//! adapter, which routes to OpenRouter via Rig — the same gateway and auth
//! surface (`OPENROUTER_API_KEY`) the adversarial judge uses (`rig_judge.rs`).
//! This is the single standardized path: no direct-vendor SDK clients.
//!
//! `LlmModel` tiers map to OpenRouter model IDs via
//! [`JudgeModel::openrouter_model_id`]:
//!   - `Opus`   → `anthropic/claude-opus-4.7`
//!   - `Sonnet` → `openai/gpt-5.5`
//!   - `Haiku`  → `openai/gpt-5.4-nano`

use anyhow::{Context, Result};
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::openrouter;
use std::sync::Arc;

use sentinel_domain::judge::JudgeModel;
use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest};

/// OpenRouter-backed LLM client implementing the domain `LlmPort`.
#[derive(Clone)]
pub struct OpenRouterLlm {
    client: Arc<openrouter::Client>,
}

impl OpenRouterLlm {
    /// Build from `OPENROUTER_API_KEY`. `Err` if the key is unset so callers
    /// can fall back / treat `ctx.llm` as `None`.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY not set")?;
        let client = openrouter::Client::new(&key)
            .map_err(|e| anyhow::anyhow!("failed to build OpenRouter client: {e}"))?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Map a domain `LlmModel` tier to its OpenRouter model ID, reusing the
    /// single source of truth in `JudgeModel`.
    fn model_id(model: LlmModel) -> &'static str {
        let judge = match model {
            LlmModel::Haiku => JudgeModel::Codex,
            LlmModel::Sonnet => JudgeModel::Sonnet,
            LlmModel::Opus => JudgeModel::Opus,
        };
        judge.openrouter_model_id()
    }
}

#[async_trait::async_trait]
impl LlmPort for OpenRouterLlm {
    async fn complete(&self, request: LlmRequest) -> Result<String> {
        let model_id = Self::model_id(request.model);
        let agent = AgentBuilder::new(self.client.completion_model(model_id)).build();
        agent
            .prompt(request.prompt)
            .await
            .map_err(|e| anyhow::anyhow!("OpenRouter completion ({model_id}): {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_opus_to_openrouter_id() {
        assert_eq!(
            OpenRouterLlm::model_id(LlmModel::Opus),
            "anthropic/claude-opus-4.7"
        );
    }

    #[test]
    fn maps_all_tiers() {
        assert_eq!(OpenRouterLlm::model_id(LlmModel::Haiku), "openai/gpt-5.4-nano");
        assert_eq!(OpenRouterLlm::model_id(LlmModel::Sonnet), "openai/gpt-5.5");
        assert_eq!(OpenRouterLlm::model_id(LlmModel::Opus), "anthropic/claude-opus-4.7");
    }

    #[test]
    fn from_env_errs_without_key() {
        // Save/clear/restore to avoid clobbering a real key in the env.
        let saved = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("OPENROUTER_API_KEY");
        assert!(OpenRouterLlm::from_env().is_err());
        if let Some(k) = saved {
            std::env::set_var("OPENROUTER_API_KEY", k);
        }
    }
}
