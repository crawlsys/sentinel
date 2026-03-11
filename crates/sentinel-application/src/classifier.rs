//! AI Skill Classifier
//!
//! Uses regex pre-match + Sonnet 4.6 to classify user messages
//! into skills. Regex runs first (~0.1ms); AI runs only on ambiguous matches.

use anyhow::Result;

use sentinel_domain::routing::RegexRouter;

/// Port for AI classification — infrastructure implements this
#[async_trait::async_trait]
pub trait AiClassifier: Send + Sync {
    /// Classify a message into a skill using AI
    async fn classify(&self, message: &str, candidates: &[String]) -> Result<Option<String>>;
}

/// Hybrid classifier: regex first, AI fallback
pub struct SkillClassifier {
    /// Regex router for fast pre-match
    router: RegexRouter,

    /// AI classifier for ambiguous cases
    ai: Option<Box<dyn AiClassifier>>,
}

impl SkillClassifier {
    pub fn new(router: RegexRouter, ai: Option<Box<dyn AiClassifier>>) -> Self {
        Self { router, ai }
    }

    /// Classify a message — returns skill name or None
    pub async fn classify(&self, message: &str) -> Result<Option<String>> {
        // 1. Try regex pre-match
        if let Some(m) = self.router.route(message) {
            if m.strong {
                return Ok(Some(m.skill));
            }

            // Weak regex match — use AI to confirm if available
            if let Some(ai) = &self.ai {
                let candidates = self
                    .router
                    .route_all(message)
                    .into_iter()
                    .map(|m| m.skill)
                    .collect::<Vec<_>>();
                return ai.classify(message, &candidates).await;
            }

            // No AI, use weak match as-is
            return Ok(Some(m.skill));
        }

        // 2. No regex match — try AI classifier with empty candidates
        if let Some(ai) = &self.ai {
            return ai.classify(message, &[]).await;
        }

        Ok(None)
    }
}
