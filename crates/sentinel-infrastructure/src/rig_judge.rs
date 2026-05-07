//! Adversarial AI Judge via `OpenRouter`
//!
//! Pluggable judge backend that routes every `JudgeModel` tier to the
//! corresponding model on OpenRouter — single gateway, single auth surface,
//! automatic provider failover. The judge should never be the same model
//! as the defendant: the `JudgeModel` enum lives in `sentinel-domain` so
//! step configs declare their tier, and this layer dispatches by enum
//! variant via `JudgeModel::openrouter_model_id()`.
//!
//! Default tier is `Kimi` (Moonshot K2.6) — OSS frontier with the best
//! agentic-coding accuracy at 4-10x lower blended price than the closed
//! frontier, plus Eastern training-distribution diversity to avoid the
//! shared-blind-spot failure mode of all-Western-closed judging.
//!
//! Reads `OPENROUTER_API_KEY`.

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::openrouter;
use std::sync::Arc;
use tracing::{debug, info};

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

/// Type-erased prompt function: (model_id, system, `user_msg`) -> response text.
/// The model id is now a runtime parameter so a single provider serves every
/// `JudgeModel` tier.
type PromptFn =
    Arc<dyn Fn(String, String, String) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// The OpenRouter-backed adversarial judge provider
struct JudgeProvider {
    prompt_fn: PromptFn,
}

impl JudgeProvider {
    /// Initialize from `OPENROUTER_API_KEY` env var.
    fn from_env() -> Result<Self> {
        let key = std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY not set")?;
        let client = Arc::new(
            openrouter::Client::new(&key)
                .map_err(|e| anyhow::anyhow!("Failed to build OpenRouter judge client: {e}"))?,
        );
        Ok(Self {
            prompt_fn: Arc::new(move |model_id, system, user_msg| {
                let client = client.clone();
                Box::pin(async move {
                    let agent = AgentBuilder::new(client.completion_model(&model_id))
                        .preamble(&system)
                        .build();
                    let result: Result<String, _> = agent.prompt(user_msg).await;
                    result.map_err(|e| anyhow::anyhow!("OpenRouter judge ({model_id}): {e}"))
                })
            }),
        })
    }

    /// Send a judge prompt to the model identified by `model_id` and parse
    /// the JSON verdict. The model id comes from
    /// [`JudgeModel::openrouter_model_id`](sentinel_domain::judge::JudgeModel::openrouter_model_id).
    async fn judge(
        &self,
        model_id: &str,
        system: &str,
        user_msg: &str,
    ) -> Result<JudgeVerdict> {
        let text = (self.prompt_fn)(
            model_id.to_string(),
            system.to_string(),
            user_msg.to_string(),
        )
        .await?;
        debug!(
            provider = "openrouter",
            model = model_id,
            response_len = text.len(),
            "Adversarial judge response received"
        );

        serde_json::from_str::<JudgeVerdict>(&text)
            .or_else(|_| extract_json_from_markdown(&text))
            .map(sentinel_domain::JudgeVerdict::sanitized) // Attack #127: clamp confidence to [0.0, 1.0]
            .context("Failed to parse judge verdict JSON")
    }
}

/// Adversarial judge dispatching every `JudgeModel` tier through OpenRouter.
///
/// Default tier is Kimi K2.6 (Moonshot, OSS frontier) — different family
/// from the Anthropic models that typically generate the work, plus
/// Eastern training-distribution diversity for adversarial review.
/// Critical-tier work pairs Kimi+Sonnet (or +Opus) — see Stage B
/// follow-up commit.
pub struct MultiModelJudge {
    judge: Option<JudgeProvider>,
}

impl MultiModelJudge {
    /// Initialize from environment variables.
    pub fn from_env() -> Self {
        let judge = JudgeProvider::from_env().ok();

        if judge.is_none() {
            eprintln!(
                "[sentinel] WARNING: No AI judge available. \
                 Set OPENROUTER_API_KEY for adversarial Kimi K2.6 / GPT-5.4 / Opus 4.7 judge. \
                 All proof submissions will fail until configured."
            );
        }
        info!(
            provider = if judge.is_some() {
                "openrouter"
            } else {
                "none"
            },
            default_tier = %JudgeModel::default_review_tier(),
            "Adversarial judge initialized — pluggable across Haiku/Kimi/Sonnet/Opus tiers"
        );

        Self { judge }
    }

    /// Returns `true` if the judge provider is available.
    pub const fn has_any_provider(&self) -> bool {
        self.judge.is_some()
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
        let provider = self.judge.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Adversarial judge not available — set OPENROUTER_API_KEY")
        })?;

        let model_id = model.openrouter_model_id();
        info!(
            provider = "openrouter",
            model = model_id,
            tier = ?model,
            skill = skill,
            phase = phase_id,
            "Adversarial judge evaluation"
        );

        let system = format!(
            "You are an ADVERSARIAL AI judge. Your job is to RIGOROUSLY evaluate whether \
             evidence actually proves that work was completed — not just claimed.\n\
             \n\
             You are intentionally a DIFFERENT model from the one that generated the work. \
             Do NOT give the benefit of the doubt. Be skeptical. Look for:\n\
             - Claims without proof (\"I did X\" without showing X was done)\n\
             - Superficial evidence (test output that doesn't actually test the feature)\n\
             - Missing edge cases or error handling\n\
             - Partial completion presented as full completion\n\
             - Tests that pass trivially (testing the wrong thing)\n\
             \n\
             If the evidence is genuinely sufficient, say so — but set a HIGH bar.\n\
             Confidence above 0.8 should mean you found STRONG evidence, not just \"looks OK.\"\n\
             \n\
             Respond with ONLY valid JSON in this exact format:\n\
             {{\"sufficient\": true/false, \"confidence\": 0.0-1.0, \"reasoning\": \"...\", \
             \"requested_evidence\": null or [\"...\", ...]}}\n\n\
             Skill: {skill}\n\
             Phase: {phase_id}\n\
             Phase objectives: {phase_objectives}"
        );

        let evidence_json = serde_json::to_string_pretty(evidence)?;
        let user_msg =
            format!("Evaluate this evidence for the '{phase_id}' phase:\n\n{evidence_json}");

        provider.judge(model_id, &system, &user_msg).await
    }
}

/// Extract the first balanced JSON object from text.
fn extract_json_from_markdown(text: &str) -> Result<JudgeVerdict, serde_json::Error> {
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
                                return Ok(v.sanitized());
                            }
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
    fn test_adversarial_judge_no_key() {
        let judge = MultiModelJudge { judge: None };
        assert!(!judge.has_any_provider());
    }
}
