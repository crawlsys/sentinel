//! AI Skill Classifier via Rig LLM framework
//!
//! Uses Claude Opus 4.7 (latest) via `OpenRouter` for intent classification.
//! The classifier receives the user's message + a compact skill catalog
//! and returns the best-matching skill name (or "none").

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::openrouter;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Claude Opus 4.7 (latest) via `OpenRouter`
const OPENROUTER_MODEL: &str = "anthropic/claude-opus-4-7";

/// Type-erased prompt function
type PromptFn = Arc<dyn Fn(String, String) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// Rig-backed AI classifier for skill routing
pub struct RigClassifier {
    prompt_fn: PromptFn,
    provider_name: &'static str,
}

impl RigClassifier {
    /// Initialize from environment — reads `OPENROUTER_API_KEY`.
    /// Returns None if the key is not set.
    pub fn from_env() -> Option<Self> {
        match Self::openrouter() {
            Ok(classifier) => {
                info!("AI classifier initialized: Claude Opus 4.7 via OpenRouter");
                Some(classifier)
            }
            Err(e) => {
                warn!(error = %e, "Failed to initialize OpenRouter classifier — OPENROUTER_API_KEY required");
                None
            }
        }
    }

    fn openrouter() -> Result<Self> {
        let key = std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY not set")?;
        let client = Arc::new(
            openrouter::Client::new(&key)
                .map_err(|e| anyhow::anyhow!("Failed to build OpenRouter client: {e}"))?,
        );
        Ok(Self {
            prompt_fn: Arc::new(move |system, user_msg| {
                let client = client.clone();
                Box::pin(async move {
                    let agent = AgentBuilder::new(client.completion_model(OPENROUTER_MODEL))
                        .preamble(&system)
                        .build();
                    agent
                        .prompt(user_msg)
                        .await
                        .map_err(|e| anyhow::anyhow!("OpenRouter classifier: {e}"))
                })
            }),
            provider_name: "openrouter",
        })
    }

    /// Classify a user message against the skill catalog.
    /// Returns the skill name or "none".
    pub async fn classify(
        &self,
        message: &str,
        skill_catalog: &str,
        candidates: &[String],
    ) -> Result<String> {
        let system = build_system_prompt(skill_catalog);

        let user_msg = if candidates.is_empty() {
            format!("User message: {message}")
        } else {
            format!(
                "User message: {message}\n\nPre-match candidates: {}",
                candidates.join(", ")
            )
        };

        debug!(
            provider = self.provider_name,
            message_len = message.len(),
            candidates = ?candidates,
            "Classifying skill"
        );

        let response = (self.prompt_fn)(system, user_msg).await?;
        let skill = response.trim().to_lowercase();

        // Validate response is a single skill name or "none"
        let skill = skill.trim_matches('"').trim_matches('`').trim().to_string();

        debug!(
            provider = self.provider_name,
            result = %skill,
            "Classification result"
        );

        Ok(skill)
    }
}

/// Maximum time to wait for AI classifier response.
/// Opus is slower (~1-2s) than Cerebras (~200ms) so give it 5s.
const CLASSIFIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[async_trait::async_trait]
impl sentinel_application::classifier::AiClassifier for RigClassifier {
    async fn classify(&self, message: &str, candidates: &[String]) -> Result<Option<String>> {
        let catalog = build_skill_catalog();

        match tokio::time::timeout(
            CLASSIFIER_TIMEOUT,
            self.classify(message, &catalog, candidates),
        )
        .await
        {
            Ok(Ok(skill)) if skill == "none" || skill.is_empty() => Ok(None),
            Ok(Ok(skill)) => Ok(Some(skill)),
            Ok(Err(e)) => {
                warn!(error = %e, "AI classification failed");
                Ok(None)
            }
            Err(_) => {
                warn!(
                    timeout_secs = CLASSIFIER_TIMEOUT.as_secs(),
                    provider = self.provider_name,
                    "AI classifier timed out"
                );
                Ok(None)
            }
        }
    }
}

/// Build the system prompt for the classifier
fn build_system_prompt(skill_catalog: &str) -> String {
    format!(
        r#"You are a skill router for an AI coding assistant. Given a user message, determine which skill (if any) should handle it.

RULES:
1. Respond with ONLY the skill name (lowercase, e.g. "linear") or "none"
2. No explanation, no punctuation, no quotes — just the bare skill name
3. "none" means general conversation that no skill should handle
4. Be precise — "fix the router" in a coding context means debug/refactor, NOT the "internet" skill
5. "router" in software context (skill router, mcp-router, React Router) is NEVER the "internet" skill
6. The "internet" skill is ONLY for physical network hardware (ATT gateway, Netgear router, port forwarding, DHCP)
7. The "execute" skill is ONLY for explicit "do it" / "build it" / "implement this" commands, NOT for general affirmative responses
8. Slash commands: /commit → commit, /test → test, /review → review, /plan → plan, /pr → pr, /debug → debug, /explore → explore, /session → session, /skills → skills. Handle typos: /commti → commit, /reveiw → review, /tset → test. Slash commands with args: "/memory search DDD" → memory
9. Short affirmative responses ("yes", "y", "ok", "sure", "do it", "keep going", "continue", "all", "lets go", "keep rolling", "next") are ALWAYS "none"
10. When ambiguous, prefer "none" over a wrong match

SKILL CATALOG:
{skill_catalog}"#
    )
}

/// Build skill catalog from ~/.claude/skills/ directory
pub fn build_skill_catalog() -> String {
    let skills_dir = match crate::paths::home_root() {
        Some(h) => h.join(".claude").join("skills"),
        None => return String::from("(no skills directory found)"),
    };

    let mut entries = Vec::new();

    if let Ok(read_dir) = std::fs::read_dir(&skills_dir) {
        let mut dirs: Vec<_> = read_dir
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                let ft = match e.file_type() {
                    Ok(ft) => ft,
                    Err(_) => return false,
                };
                if ft.is_symlink() {
                    match e.path().canonicalize() {
                        Ok(real) => {
                            let skills_canonical = skills_dir
                                .canonicalize()
                                .unwrap_or_else(|_| skills_dir.clone());
                            real.starts_with(&skills_canonical) && real.is_dir()
                        }
                        Err(_) => false,
                    }
                } else {
                    ft.is_dir()
                }
            })
            .collect();
        dirs.sort_by_key(std::fs::DirEntry::file_name);

        for dir in dirs {
            let skill_md = dir.path().join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                let name = dir.file_name().to_string_lossy().to_string();
                let desc = extract_description(&content);
                let keywords = extract_keywords(&content);
                if let Some(desc) = desc {
                    let mut entry = format!("- {name}: {desc}");
                    if let Some(kw) = keywords {
                        use std::fmt::Write as _;
                        let _ = write!(entry, " [keywords: {kw}]");
                    }
                    entries.push(entry);
                } else {
                    entries.push(format!("- {name}"));
                }
            }
        }
    }

    if entries.is_empty() {
        "(no skills found)".to_string()
    } else {
        entries.join("\n")
    }
}

/// Extract description from SKILL.md frontmatter
fn extract_description(content: &str) -> Option<String> {
    if !content.starts_with("---") {
        return None;
    }
    let after_first = &content[3..];
    let end = after_first.find("---")?;
    let frontmatter = &after_first[..end];

    let desc_start = frontmatter.find("description:")?;
    let after_desc = &frontmatter[desc_start + "description:".len()..];
    let trimmed = after_desc.trim_start();

    if let Some(stripped) = trimmed.strip_prefix('>') {
        let lines: Vec<&str> = stripped
            .lines()
            .skip_while(|l| l.trim().is_empty())
            .map(str::trim)
            .take_while(|l| {
                !l.is_empty()
                    && !l.starts_with("keywords")
                    && !l.starts_with("allowed")
                    && !l.starts_with("version")
                    && !l.starts_with("icon")
            })
            .collect();
        let desc = lines.join(" ").trim().to_string();
        if desc.is_empty() {
            None
        } else {
            Some(desc)
        }
    } else {
        let line = trimmed.lines().next()?;
        let desc = line.trim().trim_matches('"').to_string();
        if desc.is_empty() {
            None
        } else {
            Some(desc)
        }
    }
}

/// Extract keywords from SKILL.md frontmatter
fn extract_keywords(content: &str) -> Option<String> {
    if !content.starts_with("---") {
        return None;
    }
    let after_first = &content[3..];
    let end = after_first.find("---")?;
    let frontmatter = &after_first[..end];

    let kw_start = frontmatter.find("keywords:")?;
    let after_kw = &frontmatter[kw_start + "keywords:".len()..];
    let line = after_kw.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_description_multiline() {
        let content = r#"---
name: internet
description: >
  ATT BGW320 gateway and Netgear RS600 router management via MCP. 60 tools covering
  health checks, network overview, config management, port forwarding.
keywords: "router", "network"
---
"#;
        let desc = extract_description(content).unwrap();
        assert!(desc.contains("ATT BGW320"));
        assert!(desc.contains("port forwarding"));
    }

    #[test]
    fn test_extract_description_single_line() {
        let content = r#"---
name: git
description: "Smart git workflows with conventional commits"
version: 1.0.0
---
"#;
        let desc = extract_description(content).unwrap();
        assert!(desc.contains("git workflows"));
    }

    #[test]
    fn test_extract_keywords() {
        let content = r#"---
name: internet
description: test
keywords: "router", "network", "att"
version: 1.0.0
---
"#;
        let kw = extract_keywords(content).unwrap();
        assert!(kw.contains("router"));
        assert!(kw.contains("network"));
    }

    #[test]
    fn test_build_system_prompt_contains_rules() {
        let prompt = build_system_prompt("- test: run tests");
        assert!(prompt.contains("internet"));
        assert!(prompt.contains("execute"));
        assert!(prompt.contains("none"));
        assert!(prompt.contains("Short affirmative"));
    }

    #[test]
    fn test_no_frontmatter() {
        let content = "# No frontmatter here";
        assert!(extract_description(content).is_none());
        assert!(extract_keywords(content).is_none());
    }
}
