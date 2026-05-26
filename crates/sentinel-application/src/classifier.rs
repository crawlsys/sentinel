//! AI Skill Classifier
//!
//! Uses Claude Opus 4.6 to classify user messages into skills.
//! Pure AI — no regex patterns. Opus handles everything.

use anyhow::Result;

/// Port for AI classification — infrastructure implements this
#[async_trait::async_trait]
pub trait AiClassifier: Send + Sync {
    /// Classify a message into a skill using AI.
    /// Returns `Some(skill_name)` or None for general conversation.
    async fn classify(&self, message: &str, candidates: &[String]) -> Result<Option<String>>;
}
