//! Local-LLM `LlmPort` — OpenAI-compatible endpoint at any base URL.
//!
//! Speaks to vLLM, Ollama (`/v1/*` compat path), llama.cpp's
//! `--api`, LM Studio — anything that implements the OpenAI
//! `/v1/chat/completions` schema. Same crate as `openrouter_llm.rs`
//! (the cloud counterpart); selection between them lives in
//! `LlmRouter` (see `llm_router.rs`).
//!
//! Env contract (matches the homelab `ollama-research` chart and
//! the existing `llm_scorer_runtime.rs` Ollama path):
//!   - `OLLAMA_HOST`        — base host, e.g. `http://ollama-research:11434`.
//!                            Defaults to `http://localhost:11434`. The
//!                            `/v1` suffix is appended automatically.
//!   - `OLLAMA_API_KEY`     — auth token for cloud-mode Ollama or any
//!                            vLLM with auth enabled. Optional; defaults
//!                            to the literal `"sentinel-local"` (Ollama
//!                            ignores it for in-cluster traffic, and
//!                            rig's OpenAI client refuses an empty key).
//!   - `OLLAMA_MODEL_HAIKU` / `_SONNET` / `_OPUS` / `_CODEX` / `_KIMI`
//!                          — per-tier model ID. Defaults to a single
//!                            model across all tiers (`qwen3.5-35b-a3b`,
//!                            matching the homelab chart hint) because
//!                            `OLLAMA_MAX_LOADED_MODELS=1` evicts
//!                            others on demand and serving a single
//!                            model amortises the cold-start cost.

use anyhow::{Context, Result};
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::openai;
use std::sync::Arc;

use sentinel_domain::ports::{LlmModel, LlmPort, LlmRequest};

/// Default base host. The chart's NodePort is 31435 on cluster
/// nodes; in-cluster service is 11434 on
/// `ollama-research.ollama-research.svc.cluster.local`. Operators
/// pick the right one for their network via `OLLAMA_HOST`.
const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";

/// Placeholder API key for un-authed local Ollama. The OpenAI
/// client rejects empty strings; the literal value here is
/// inert to Ollama itself but satisfies the builder.
const DEFAULT_LOCAL_API_KEY: &str = "sentinel-local";

/// Default model for every tier. Maps to the homelab
/// `ollama-research` deployment on nighttime —
/// `qwen3-coder:30b` (coming online via NodePort 31435). With
/// `OLLAMA_MAX_LOADED_MODELS=1` only one model lives in VRAM at a
/// time, so tier-specialised model IDs would just thrash the
/// load/evict cycle. Operators with multi-model deployments
/// (vLLM, larger boxes) override per-tier via the env vars.
const DEFAULT_MODEL: &str = "qwen3-coder:30b";

/// Local-LLM client. Wraps a rig OpenAI-compatible client pointed
/// at the operator's local endpoint.
#[derive(Clone)]
pub struct LocalLlm {
    client: Arc<openai::Client>,
    haiku: String,
    sonnet: String,
    opus: String,
    codex: String,
    kimi: String,
    /// Base URL we built with — kept for diagnostics + health
    /// checks. The OpenAI client doesn't expose it back to us.
    base_url: String,
}

impl LocalLlm {
    /// Build from environment. Construction always succeeds when
    /// the URL parses — reachability is a separate concern handled
    /// by `LlmRouter::probe_local`. Returns `Err` only when rig's
    /// builder itself rejects the inputs (malformed URL, etc).
    pub fn from_env() -> Result<Self> {
        let host = std::env::var("OLLAMA_HOST")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_HOST.to_string());
        let base_url = format!("{}/v1", host.trim_end_matches('/'));
        let api_key = std::env::var("OLLAMA_API_KEY")
            .unwrap_or_else(|_| DEFAULT_LOCAL_API_KEY.to_string());
        let client = openai::Client::builder()
            .api_key(&api_key)
            .base_url(&base_url)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build local LLM client (base_url={base_url}): {e}"))
            .context("local LLM client build")?;
        Ok(Self {
            client: Arc::new(client),
            haiku: std::env::var("OLLAMA_MODEL_HAIKU").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            sonnet: std::env::var("OLLAMA_MODEL_SONNET").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            opus: std::env::var("OLLAMA_MODEL_OPUS").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            codex: std::env::var("OLLAMA_MODEL_CODEX").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            kimi: std::env::var("OLLAMA_MODEL_KIMI").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            base_url,
        })
    }

    /// Read-only access to the configured base URL — used by the
    /// router's health probe to hit `/v1/models` at the right
    /// host before deciding to route traffic here.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn model_id(&self, model: LlmModel) -> &str {
        match model {
            LlmModel::Haiku => &self.haiku,
            LlmModel::Sonnet => &self.sonnet,
            LlmModel::Opus => &self.opus,
            LlmModel::Codex => &self.codex,
            LlmModel::Kimi => &self.kimi,
        }
    }
}

#[async_trait::async_trait]
impl LlmPort for LocalLlm {
    async fn complete(&self, request: LlmRequest) -> Result<String> {
        let model_id = self.model_id(request.model).to_string();
        let agent = AgentBuilder::new(self.client.completion_model(&model_id)).build();
        agent
            .prompt(request.prompt)
            .await
            .map_err(|e| anyhow::anyhow!("local LLM completion ({model_id} @ {}): {e}", self.base_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise env-mutating tests so concurrent runs don't race.
    /// All tests in this file touch `OLLAMA_*` env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_succeeds_with_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved_host = std::env::var("OLLAMA_HOST").ok();
        let saved_key = std::env::var("OLLAMA_API_KEY").ok();
        std::env::remove_var("OLLAMA_HOST");
        std::env::remove_var("OLLAMA_API_KEY");
        let result = LocalLlm::from_env();
        if let Some(v) = saved_host { std::env::set_var("OLLAMA_HOST", v); }
        if let Some(v) = saved_key { std::env::set_var("OLLAMA_API_KEY", v); }
        let llm = result.expect("default construction must succeed");
        assert!(llm.base_url().ends_with("/v1"));
        assert!(llm.base_url().contains("localhost:11434"));
    }

    #[test]
    fn base_url_appends_v1_and_trims_trailing_slash() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("OLLAMA_HOST").ok();
        std::env::set_var("OLLAMA_HOST", "http://nighttime:11434/");
        let llm = LocalLlm::from_env().unwrap();
        assert_eq!(llm.base_url(), "http://nighttime:11434/v1");
        match saved {
            Some(v) => std::env::set_var("OLLAMA_HOST", v),
            None => std::env::remove_var("OLLAMA_HOST"),
        }
    }

    #[test]
    fn model_overrides_take_effect_per_tier() {
        let _g = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("OLLAMA_MODEL_HAIKU").ok();
        std::env::set_var("OLLAMA_MODEL_HAIKU", "custom/haiku");
        let llm = LocalLlm::from_env().unwrap();
        assert_eq!(llm.model_id(LlmModel::Haiku), "custom/haiku");
        // Other tiers fall through to default.
        assert_eq!(llm.model_id(LlmModel::Opus), DEFAULT_MODEL);
        match saved {
            Some(v) => std::env::set_var("OLLAMA_MODEL_HAIKU", v),
            None => std::env::remove_var("OLLAMA_MODEL_HAIKU"),
        }
    }

    #[test]
    fn defaults_cover_all_tiers() {
        let _g = ENV_LOCK.lock().unwrap();
        let llm = LocalLlm::from_env().unwrap();
        for tier in [
            LlmModel::Haiku,
            LlmModel::Sonnet,
            LlmModel::Opus,
            LlmModel::Codex,
            LlmModel::Kimi,
        ] {
            assert!(!llm.model_id(tier).is_empty());
        }
    }
}
