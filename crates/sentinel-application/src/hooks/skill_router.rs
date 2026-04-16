//! Skill Router Hook
//!
//! Routes every user message to the appropriate skill via Claude Opus 4.6.
//! Pure AI classification — no regex patterns. Opus handles slash commands,
//! natural language, typos, and everything in between.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::fs;
use std::io::Write;

/// Extract the activation banner from a skill's SKILL.md file.
/// Returns the content between `## Activation Banner` and the next `##` heading,
/// including the code-fenced banner block.
fn extract_banner(skill: &str) -> Option<String> {
    let skill_path = dirs::home_dir()?
        .join(".claude")
        .join("skills")
        .join(skill)
        .join("SKILL.md");

    let content = fs::read_to_string(skill_path).ok()?;

    // Find the Activation Banner section
    let banner_start = content.find("## Activation Banner")?;
    let after_header = &content[banner_start..];

    // Find the code block within the banner section
    let code_start = after_header.find("```")?;
    let code_body = &after_header[code_start + 3..];
    let code_end = code_body.find("```")?;

    // Extract just the content inside the code fence (skip the opening line like "```\n")
    let inner = &code_body[..code_end];
    // Strip the optional language identifier on the first line of the code fence
    let inner = if let Some(nl) = inner.find('\n') {
        &inner[nl + 1..]
    } else {
        inner
    };

    let trimmed = inner.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Directory for telemetry state files — inside sentinel's protected config dir
/// instead of world-writable temp_dir(). Prevents other processes/users from
/// injecting fake skill names or run IDs. (Attack #51)
fn telemetry_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("sentinel").join("telemetry"))
}

/// Write telemetry state files so skill_telemetry can track the execution.
fn write_telemetry_state(skill: &str, run_id: &str) {
    let dir = match telemetry_dir() {
        Some(d) => d,
        None => return,
    };
    let _ = fs::create_dir_all(&dir);

    // Current skill name
    let _ = fs::write(dir.join("claude-current-skill"), skill);

    // Run ID for correlation
    let _ = fs::write(dir.join("claude-skill-run-id"), run_id);

    // Start timestamp (epoch ms) for duration calculation
    let now_ms = chrono::Utc::now().timestamp_millis();
    let _ = fs::write(dir.join("claude-skill-start-time"), now_ms.to_string());
}

/// Append a routing entry to metrics/routing.jsonl
fn write_routing_entry(skill: &str, run_id: &str, source: &str, input: &HookInput, prompt: &str) {
    let metrics_dir = match dirs::home_dir() {
        Some(h) => super::metrics_dir(&h),
        None => return,
    };
    let _ = fs::create_dir_all(&metrics_dir);

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let ts = chrono::Utc::now().to_rfc3339();

    // Truncate prompt for logging (first 100 chars)
    let prompt_short: String = prompt.chars().take(100).collect();

    let entry = serde_json::json!({
        "run_id": run_id,
        "session_id": session_id,
        "event": "skill_routed",
        "skill": skill,
        "source": source,
        "status": "started",
        "cwd": cwd,
        "prompt": prompt_short,
        "ts": ts,
    });

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(metrics_dir.join("routing.jsonl"))
    {
        let _ = writeln!(
            file,
            "{}",
            serde_json::to_string(&entry).unwrap_or_default()
        );
    }
}

/// Process a skill-router hook event via Opus AI classification.
///
/// ALL routing goes through Claude Opus 4.6 — slash commands, natural
/// language, typos, everything. No regex fallback. Opus understands
/// "/commti" is a typo for "commit" and "migrate hooks to ports" means
/// the ddd-hexagonal or refactor skill.
///
/// Validates AI return against actual skill directories on disk.
pub async fn process(
    input: &HookInput,
    classifier: Option<&dyn crate::classifier::AiClassifier>,
) -> HookOutput {
    let prompt = match &input.prompt {
        Some(p) if !p.trim().is_empty() => p,
        _ => return HookOutput::allow(),
    };

    // AI classification — Opus classifies everything
    if let Some(ai) = classifier {
        match ai.classify(prompt, &[]).await {
            Ok(Some(skill)) => {
                // Validate: skill directory must exist on disk
                if is_valid_skill(&skill) {
                    let source = if prompt.trim().starts_with('/') { "ai-slash" } else { "ai" };
                    return build_match_output(&skill, input, prompt, source);
                }
                tracing::warn!(
                    returned_skill = %skill,
                    "AI returned skill that doesn't exist on disk — ignoring"
                );
            }
            Ok(None) => {
                // AI says no skill matches — general conversation
            }
            Err(e) => {
                tracing::warn!(error = %e, "AI classifier failed — no routing");
            }
        }
    } else {
        tracing::warn!("No AI classifier available — skill routing disabled for this message");
    }

    build_no_match_output()
}

/// Check if a skill name corresponds to an actual skill directory with SKILL.md
fn is_valid_skill(skill: &str) -> bool {
    let skills_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("skills"),
        None => return false,
    };
    skills_dir.join(skill).join("SKILL.md").exists()
}

/// Build output for a matched skill
fn build_match_output(skill: &str, input: &HookInput, prompt: &str, source: &str) -> HookOutput {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let pid = std::process::id();
    let run_id = format!("{}-{}", now_ms, pid % 100000);

    write_telemetry_state(skill, &run_id);
    write_routing_entry(skill, &run_id, source, input, prompt);

    // Log the routing source for diagnostics
    tracing::info!(skill = skill, source = source, "Skill routed");

    let skill_path = format!("~/.claude/skills/{}/SKILL.md", skill);
    let banner = extract_banner(skill);

    let context = if let Some(ref b) = banner {
        format!(
            "{}\n\n[Skill Router] Detected skill: {}. \
             MANDATORY: You MUST Read(\"{}\") BEFORE responding. \
             This is a non-negotiable requirement.",
            b, skill, skill_path
        )
    } else {
        format!(
            "[Skill Router] Detected skill: {}. \
             MANDATORY: You MUST Read(\"{}\") BEFORE responding. \
             This is a non-negotiable requirement.",
            skill, skill_path
        )
    };

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

/// Build output for no match
pub fn build_no_match_output() -> HookOutput {
    if let Some(dir) = telemetry_dir() {
        let _ = fs::remove_file(dir.join("claude-current-skill"));
        let _ = fs::remove_file(dir.join("claude-skill-run-id"));
        let _ = fs::remove_file(dir.join("claude-skill-start-time"));
    }

    HookOutput::inject_context(
        HookEvent::UserPromptSubmit,
        "[Skill Router] No skill matched — general conversation mode.".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_match_output_has_context() {
        let output = build_no_match_output();
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("No skill matched"));
    }

    #[test]
    fn test_is_valid_skill_rejects_garbage() {
        // A skill name with path traversal should not resolve
        assert!(!is_valid_skill("../../../etc/passwd"));
        assert!(!is_valid_skill("nonexistent-skill-xyz"));
    }

    #[tokio::test]
    async fn test_process_no_classifier_returns_no_match() {
        let input = HookInput {
            prompt: Some("run the tests".to_string()),
            ..Default::default()
        };
        let output = process(&input, None).await;
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("No skill matched"));
    }

    #[tokio::test]
    async fn test_process_empty_prompt_returns_allow() {
        let input = HookInput {
            prompt: Some("".to_string()),
            ..Default::default()
        };
        let output = process(&input, None).await;
        assert!(output.hook_specific_output.is_none());
    }

    #[tokio::test]
    async fn test_process_no_prompt_returns_allow() {
        let input = HookInput::default();
        let output = process(&input, None).await;
        assert!(output.hook_specific_output.is_none());
    }
}
