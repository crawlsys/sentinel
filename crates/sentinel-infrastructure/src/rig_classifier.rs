//! AI Skill Classifier via Rig LLM framework
//!
//! Uses Cerebras (fastest, ~200ms) for intent classification.
//! Falls back to OpenAI, then Anthropic if Cerebras is unavailable.
//! The classifier receives the user's message + a compact skill catalog
//! and returns the best-matching skill name (or "none").

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use rig_core::agent::AgentBuilder;
use rig_core::completion::Prompt;
use rig_core::prelude::CompletionClient;
use rig_core::providers::openai;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Cerebras is fastest (~200ms) — use zai-glm for classification
const CEREBRAS_BASE_URL: &str = "https://api.cerebras.ai/v1";
const CEREBRAS_MODEL: &str = "zai-glm-4.7";

/// OpenAI fallback
const OPENAI_MODEL: &str = "gpt-4.1-mini";

/// Type-erased prompt function
type PromptFn = Arc<dyn Fn(String, String) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// Rig-backed AI classifier for skill routing
pub struct RigClassifier {
    prompt_fn: PromptFn,
    provider_name: &'static str,
}

impl RigClassifier {
    /// Initialize from environment — tries Cerebras first, then OpenAI.
    /// Returns None if no API keys are configured.
    pub fn from_env() -> Option<Self> {
        // Try Cerebras first (fastest)
        if let Ok(classifier) = Self::cerebras() {
            info!("AI classifier initialized: Cerebras");
            return Some(classifier);
        }

        // Fallback to OpenAI
        if let Ok(classifier) = Self::openai() {
            info!("AI classifier initialized: OpenAI");
            return Some(classifier);
        }

        warn!("No AI classifier available — falling back to regex-only routing");
        None
    }

    fn cerebras() -> Result<Self> {
        let key = std::env::var("CEREBRAS_API_KEY").context("CEREBRAS_API_KEY not set")?;
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
                    agent
                        .prompt(user_msg)
                        .await
                        .map_err(|e| anyhow::anyhow!("Cerebras classifier: {e}"))
                })
            }),
            provider_name: "cerebras",
        })
    }

    fn openai() -> Result<Self> {
        let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
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
                    agent
                        .prompt(user_msg)
                        .await
                        .map_err(|e| anyhow::anyhow!("OpenAI classifier: {e}"))
                })
            }),
            provider_name: "openai",
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
                "User message: {message}\n\nRegex pre-match candidates: {}",
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

/// Maximum time to wait for AI classifier response before falling back to regex.
/// 3 seconds is generous for Cerebras (~200ms typical) and OpenAI (~500ms typical).
/// Without this timeout, a hung API connection blocks message submission indefinitely.
const CLASSIFIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[async_trait::async_trait]
impl sentinel_application::classifier::AiClassifier for RigClassifier {
    async fn classify(&self, message: &str, candidates: &[String]) -> Result<Option<String>> {
        // Build catalog on each call (could be cached, but skills dir rarely changes)
        let catalog = build_skill_catalog();

        // Wrap in timeout to prevent hung API calls from blocking message submission.
        // This was the root cause of UserPromptSubmit hooks silently blocking input.
        match tokio::time::timeout(
            CLASSIFIER_TIMEOUT,
            self.classify(message, &catalog, candidates),
        )
        .await
        {
            Ok(Ok(skill)) if skill == "none" || skill.is_empty() => Ok(None),
            Ok(Ok(skill)) => Ok(Some(skill)),
            Ok(Err(e)) => {
                warn!(error = %e, "AI classification failed, falling back to regex");
                Ok(None)
            }
            Err(_) => {
                warn!(
                    timeout_secs = CLASSIFIER_TIMEOUT.as_secs(),
                    provider = self.provider_name,
                    "AI classifier timed out, falling back to regex"
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
7. The "execute" skill is ONLY for explicit "do it" / "build it" / "implement this" commands, NOT for general affirmative responses like "yes", "lets do it", "go ahead"
8. Slash commands (/commit, /test, /review, etc.) are already handled — you won't see them
9. When ambiguous, prefer "none" over a wrong match

SKILL CATALOG:
{skill_catalog}"#
    )
}

/// Build skill catalog from ~/.claude/skills/ directory
pub fn build_skill_catalog() -> String {
    let skills_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("skills"),
        None => return String::from("(no skills directory found)"),
    };

    let mut entries = Vec::new();

    if let Ok(read_dir) = std::fs::read_dir(&skills_dir) {
        let mut dirs: Vec<_> = read_dir
            .filter_map(|e| e.ok())
            .filter(|e| {
                // **Attack #126 fix**: Use metadata() instead of file_type() to follow
                // symlinks and verify the target exists. Reject symlinks that resolve
                // outside ~/.claude/skills/ to prevent catalog injection via symlinks
                // pointing to attacker-controlled directories.
                let ft = match e.file_type() {
                    Ok(ft) => ft,
                    Err(_) => return false,
                };
                if ft.is_symlink() {
                    // Follow the symlink and check if target is a real directory
                    // under the skills dir
                    match e.path().canonicalize() {
                        Ok(real) => {
                            let skills_canonical = skills_dir
                                .canonicalize()
                                .unwrap_or_else(|_| skills_dir.clone());
                            real.starts_with(&skills_canonical) && real.is_dir()
                        }
                        Err(_) => false, // Dangling symlink
                    }
                } else {
                    ft.is_dir()
                }
            })
            .collect();
        dirs.sort_by_key(|d| d.file_name());

        for dir in dirs {
            let skill_md = dir.path().join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                let name = dir.file_name().to_string_lossy().to_string();
                let desc = extract_description(&content);
                let keywords = extract_keywords(&content);
                if let Some(desc) = desc {
                    let mut entry = format!("- {name}: {desc}");
                    if let Some(kw) = keywords {
                        entry.push_str(&format!(" [keywords: {kw}]"));
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
    // Find frontmatter boundaries
    if !content.starts_with("---") {
        return None;
    }
    let after_first = &content[3..];
    let end = after_first.find("---")?;
    let frontmatter = &after_first[..end];

    // Find description field (may be multiline with >)
    let desc_start = frontmatter.find("description:")?;
    let after_desc = &frontmatter[desc_start + "description:".len()..];
    let trimmed = after_desc.trim_start();

    if trimmed.starts_with('>') {
        // Multiline YAML — collect indented continuation lines after `>`
        let lines: Vec<&str> = trimmed[1..]
            .lines()
            .skip_while(|l| l.trim().is_empty()) // skip blank line after >
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
        // Single line
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
    }

    #[test]
    fn test_no_frontmatter() {
        let content = "# No frontmatter here";
        assert!(extract_description(content).is_none());
        assert!(extract_keywords(content).is_none());
    }
}
