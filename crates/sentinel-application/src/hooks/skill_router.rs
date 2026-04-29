//! Skill Router Hook
//!
//! Routes every user message to the appropriate skill via Claude Opus 4.6.
//! Pure AI classification — no regex patterns. Opus handles slash commands,
//! natural language, typos, and everything in between.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::FileSystemPort;

/// Extract the activation banner from a skill's SKILL.md file.
/// Returns the content between `## Activation Banner` and the next `##` heading,
/// including the code-fenced banner block.
fn extract_banner(fs: &dyn FileSystemPort, skill: &str) -> Option<String> {
    let skill_path = fs
        .home_dir()?
        .join(".claude")
        .join("skills")
        .join(skill)
        .join("SKILL.md");

    let content = fs.read_to_string(&skill_path).ok()?;

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
fn telemetry_dir(fs: &dyn FileSystemPort) -> Option<std::path::PathBuf> {
    fs.home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("telemetry"))
}

/// Path to the pending-skill state file for the given session. Lives under
/// the sentinel state directory so the `skill_invocation_gate` PreToolUse
/// hook can pick it up on the next tool call. Scoped per session so two
/// concurrent agent sessions don't fight over each other's pending skills.
pub(crate) fn pending_skill_state_path(
    fs: &dyn FileSystemPort,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let dir = fs
        .home_dir()?
        .join(".claude")
        .join("sentinel")
        .join("state");
    let _ = fs.create_dir_all(&dir);
    // Hash the session id so the file name stays bounded length and
    // filesystem-safe even if the session id contains weird chars.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    let h: String = hasher.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(dir.join(format!("skill-pending-{h}.json")))
}

/// State written by `build_match_output` after a skill is detected.
/// Read by `skill_invocation_gate` on the next PreToolUse call to decide
/// whether to block.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingSkillState {
    pub skill: String,
    pub skill_path: String,
    pub detected_at: String,
    pub session_id: String,
}

/// Persist the pending-skill marker so the invocation gate can enforce
/// "you must invoke the detected skill before doing anything else."
fn write_pending_skill_state(
    fs: &dyn FileSystemPort,
    skill: &str,
    skill_path: &str,
    session_id: &str,
) {
    let path = match pending_skill_state_path(fs, session_id) {
        Some(p) => p,
        None => return,
    };
    let state = PendingSkillState {
        skill: skill.to_string(),
        skill_path: skill_path.to_string(),
        detected_at: chrono::Utc::now().to_rfc3339(),
        session_id: session_id.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = fs.write(&path, json.as_bytes());
    }
}

/// Write telemetry state files so skill_telemetry can track the execution.
fn write_telemetry_state(fs: &dyn FileSystemPort, skill: &str, run_id: &str) {
    let dir = match telemetry_dir(fs) {
        Some(d) => d,
        None => return,
    };
    let _ = fs.create_dir_all(&dir);

    // Current skill name
    let _ = fs.write(&dir.join("claude-current-skill"), skill.as_bytes());

    // Run ID for correlation
    let _ = fs.write(&dir.join("claude-skill-run-id"), run_id.as_bytes());

    // Start timestamp (epoch ms) for duration calculation
    let now_ms = chrono::Utc::now().timestamp_millis();
    let _ = fs.write(
        &dir.join("claude-skill-start-time"),
        now_ms.to_string().as_bytes(),
    );
}

/// Append a routing entry to metrics/routing.jsonl
fn write_routing_entry(
    fs: &dyn FileSystemPort,
    skill: &str,
    run_id: &str,
    source: &str,
    input: &HookInput,
    prompt: &str,
) {
    let metrics_dir = match fs.home_dir() {
        Some(h) => super::metrics_dir(&h),
        None => return,
    };
    let _ = fs.create_dir_all(&metrics_dir);

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

    let line = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());
    let _ = fs.append(&metrics_dir.join("routing.jsonl"), line.as_bytes());
}

/// Process a skill-router hook event via Opus AI classification.
///
/// Strip injected hook context (`<system-reminder>` blocks, `<channel>`
/// blocks, and standalone `[Bracketed Tag]` reminder lines) from a prompt
/// before classification.
///
/// SEN-2 fix: previously the AI classifier would route on words appearing in
/// hook-injected reminders (e.g., the literal `[Worktree Cleanup]` reminder
/// line caused the `cleanup` skill to fire). Strip those out so only the
/// actual user message body drives routing.
fn strip_hook_context(prompt: &str) -> String {
    // Remove all <system-reminder>...</system-reminder> blocks (multi-line).
    let mut buf = String::with_capacity(prompt.len());
    let mut rest = prompt;
    loop {
        let Some(start) = rest.find("<system-reminder>") else {
            break;
        };
        buf.push_str(&rest[..start]);
        let after = &rest[start + "<system-reminder>".len()..];
        if let Some(end) = after.find("</system-reminder>") {
            rest = &after[end + "</system-reminder>".len()..];
        } else {
            rest = "";
            break;
        }
    }
    buf.push_str(rest);

    // Remove all <channel ...>...</channel> blocks.
    let with_reminders_stripped = buf.clone();
    let mut buf2 = String::with_capacity(with_reminders_stripped.len());
    let mut rest = with_reminders_stripped.as_str();
    loop {
        let Some(start) = rest.find("<channel ").or_else(|| rest.find("<channel>")) else {
            break;
        };
        buf2.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(end) = after.find("</channel>") {
            rest = &after[end + "</channel>".len()..];
        } else {
            rest = "";
            break;
        }
    }
    buf2.push_str(rest);

    // Drop standalone "[Bracketed Tag] ..." reminder lines.
    buf2.lines()
        .filter(|line| {
            let t = line.trim_start();
            let stripped = t.trim_start_matches(|c: char| !c.is_ascii_alphabetic() && c != '[');
            !stripped.starts_with('[')
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// ALL routing goes through Claude Opus 4.6 — slash commands, natural
/// language, typos, everything. No regex fallback. Opus understands
/// "/commti" is a typo for "commit" and "migrate hooks to ports" means
/// the ddd-hexagonal or refactor skill.
///
/// Validates AI return against actual skill directories on disk.
pub async fn process(
    input: &HookInput,
    classifier: Option<&dyn crate::classifier::AiClassifier>,
    fs: &dyn FileSystemPort,
) -> HookOutput {
    let raw_prompt = match &input.prompt {
        Some(p) if !p.trim().is_empty() => p.as_str(),
        _ => return HookOutput::allow(),
    };

    // SEN-2: classify on the cleaned user-message body, not on injected
    // hook reminders / channel events / system-reminder blocks.
    let cleaned = strip_hook_context(raw_prompt);
    let prompt: &str = if cleaned.is_empty() {
        return HookOutput::allow();
    } else {
        &cleaned
    };

    // AI classification — Opus classifies everything
    if let Some(ai) = classifier {
        match ai.classify(prompt, &[]).await {
            Ok(Some(skill)) => {
                // Validate: skill directory must exist on disk
                if is_valid_skill(fs, &skill) {
                    let source = if prompt.trim().starts_with('/') {
                        "ai-slash"
                    } else {
                        "ai"
                    };
                    return build_match_output(fs, &skill, input, prompt, source);
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

    build_no_match_output(fs)
}

/// Check if a skill name corresponds to an actual skill directory with SKILL.md
fn is_valid_skill(fs: &dyn FileSystemPort, skill: &str) -> bool {
    let skills_dir = match fs.home_dir() {
        Some(h) => h.join(".claude").join("skills"),
        None => return false,
    };
    fs.exists(&skills_dir.join(skill).join("SKILL.md"))
}

/// Build output for a matched skill
fn build_match_output(
    fs: &dyn FileSystemPort,
    skill: &str,
    input: &HookInput,
    prompt: &str,
    source: &str,
) -> HookOutput {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let pid = std::process::id();
    let run_id = format!("{}-{}", now_ms, pid % 100000);

    write_telemetry_state(fs, skill, &run_id);
    write_routing_entry(fs, skill, &run_id, source, input, prompt);

    // Persist pending-skill state for `skill_invocation_gate` to enforce.
    // Without this, the MANDATORY message below is just a polite request
    // — Claude can ignore it and use other tools without invoking the skill.
    if let Some(session_id) = input.session_id.as_deref() {
        let skill_path_str = format!("~/.claude/skills/{}/SKILL.md", skill);
        write_pending_skill_state(fs, skill, &skill_path_str, session_id);
    }

    // Log the routing source for diagnostics
    tracing::info!(skill = skill, source = source, "Skill routed");

    let skill_path = format!("~/.claude/skills/{}/SKILL.md", skill);
    // Always render a banner — falls back to a synthesized one built from the
    // skill's frontmatter description when the SKILL.md doesn't have an
    // explicit `## Activation Banner` section. Keeps the visual cue
    // consistent across all 76+ skills regardless of authoring discipline.
    let banner = extract_banner(fs, skill).unwrap_or_else(|| synthesize_banner(fs, skill));

    let context = format!(
        "{}\n\n[Skill Router] Detected skill: {}. \
         MANDATORY: You MUST Read(\"{}\") BEFORE responding. \
         This is a non-negotiable requirement.",
        banner, skill, skill_path
    );

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

/// Build a fallback activation banner from the skill's frontmatter
/// `description` when the SKILL.md doesn't define a `## Activation Banner`
/// section. The synthesized banner is a single-line box with the skill name
/// + truncated description so the user still sees a visual cue.
fn synthesize_banner(fs: &dyn FileSystemPort, skill: &str) -> String {
    let description = read_skill_description(fs, skill).unwrap_or_default();
    let title = skill.to_uppercase();
    let subtitle = if description.is_empty() {
        "Skill detected — see SKILL.md for usage.".to_string()
    } else {
        // Hard cap so a verbose description doesn't blow up the banner.
        let trimmed = description.trim();
        if trimmed.len() > 160 {
            format!("{}…", &trimmed[..159])
        } else {
            trimmed.to_string()
        }
    };
    format!(
        "┌─────────────────────────────────────────────────────────────┐\n\
         │  📋  {title}\n\
         │  {subtitle}\n\
         └─────────────────────────────────────────────────────────────┘"
    )
}

/// Pull the `description:` field out of a SKILL.md YAML frontmatter block.
/// Returns None if the file can't be read or the frontmatter is malformed.
fn read_skill_description(fs: &dyn FileSystemPort, skill: &str) -> Option<String> {
    let skill_md = fs
        .home_dir()?
        .join(".claude")
        .join("skills")
        .join(skill)
        .join("SKILL.md");
    let content = fs.read_to_string(&skill_md).ok()?;

    // Frontmatter is delimited by `---` lines at the top of the file.
    let body = content.strip_prefix("---")?;
    let end = body.find("\n---")?;
    let frontmatter = &body[..end];

    for line in frontmatter.lines() {
        if let Some(rest) = line.strip_prefix("description:") {
            return Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}

/// Build output for no match
pub fn build_no_match_output(fs: &dyn FileSystemPort) -> HookOutput {
    if let Some(dir) = telemetry_dir(fs) {
        let _ = fs.remove_file(&dir.join("claude-current-skill"));
        let _ = fs.remove_file(&dir.join("claude-skill-run-id"));
        let _ = fs.remove_file(&dir.join("claude-skill-start-time"));
    }

    HookOutput::inject_context(
        HookEvent::UserPromptSubmit,
        "[Skill Router] No skill matched — general conversation mode.".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::StubFs;

    #[test]
    fn test_no_match_output_has_context() {
        let output = build_no_match_output(&StubFs);
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("No skill matched"));
    }

    #[test]
    fn test_strip_hook_context_removes_system_reminders() {
        // SEN-2: <system-reminder>...</system-reminder> blocks are removed.
        let input = "tell me a joke\n<system-reminder>\n[Worktree Cleanup] 4 stale worktree(s) found\n</system-reminder>";
        let cleaned = strip_hook_context(input);
        assert_eq!(cleaned, "tell me a joke");
        assert!(!cleaned.contains("Cleanup"));
    }

    #[test]
    fn test_strip_hook_context_removes_channel_blocks() {
        let input = "do the thing\n<channel source=\"sentinel\" event=\"task_completed\">noise</channel>";
        let cleaned = strip_hook_context(input);
        assert_eq!(cleaned, "do the thing");
    }

    #[test]
    fn test_strip_hook_context_removes_bracketed_reminder_lines() {
        // SEN-2: bare bracketed reminder lines are removed too.
        let input = "build the next feature\n[Worktree Cleanup] 4 stale worktree(s) found";
        let cleaned = strip_hook_context(input);
        assert_eq!(cleaned, "build the next feature");
    }

    #[test]
    fn test_strip_hook_context_keeps_pure_user_text() {
        let input = "implement the auth flow";
        let cleaned = strip_hook_context(input);
        assert_eq!(cleaned, "implement the auth flow");
    }

    #[test]
    fn test_strip_hook_context_returns_empty_when_only_hook_context() {
        let input = "<system-reminder>[Worktree Reminder] xyz</system-reminder>";
        let cleaned = strip_hook_context(input);
        assert_eq!(cleaned, "");
    }

    #[test]
    fn test_synthesize_banner_falls_back_to_default_subtitle() {
        // StubFs returns no SKILL.md content, so read_skill_description
        // yields None and synthesize_banner uses the default subtitle.
        let banner = synthesize_banner(&StubFs, "qdrant");
        assert!(banner.contains("QDRANT"));
        assert!(banner.contains("Skill detected"));
        assert!(banner.starts_with("┌"));
        assert!(banner.ends_with("┘"));
    }

    #[test]
    fn test_synthesize_banner_truncates_long_descriptions() {
        // Build an FS stub that returns a SKILL.md with a 500-char description
        struct LongDescFs;
        impl crate::hooks::FileSystemPort for LongDescFs {
            fn home_dir(&self) -> Option<std::path::PathBuf> {
                Some(std::path::PathBuf::from("/mock/home"))
            }
            fn read_to_string(&self, _: &std::path::Path) -> anyhow::Result<String> {
                let long_desc = "x".repeat(500);
                Ok(format!(
                    "---\nname: foo\ndescription: {long_desc}\n---\n# Body\n"
                ))
            }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
                Ok(vec![])
            }
            fn exists(&self, _: &std::path::Path) -> bool {
                true
            }
            fn is_dir(&self, _: &std::path::Path) -> bool {
                false
            }
            fn metadata(&self, _: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
                anyhow::bail!("nope")
            }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let banner = synthesize_banner(&LongDescFs, "foo");
        // Description is hard-capped at 160 chars + "…", so the banner
        // can't include the full 500-char string verbatim.
        assert!(!banner.contains(&"x".repeat(500)));
        assert!(banner.contains("…"));
    }

    #[test]
    fn test_read_skill_description_extracts_quoted_value() {
        struct QuotedFs;
        impl crate::hooks::FileSystemPort for QuotedFs {
            fn home_dir(&self) -> Option<std::path::PathBuf> {
                Some(std::path::PathBuf::from("/mock/home"))
            }
            fn read_to_string(&self, _: &std::path::Path) -> anyhow::Result<String> {
                Ok("---\nname: foo\ndescription: \"hello world\"\n---\n# Body\n".to_string())
            }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
                Ok(vec![])
            }
            fn exists(&self, _: &std::path::Path) -> bool {
                true
            }
            fn is_dir(&self, _: &std::path::Path) -> bool {
                false
            }
            fn metadata(&self, _: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
                anyhow::bail!("nope")
            }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            read_skill_description(&QuotedFs, "foo").as_deref(),
            Some("hello world"),
        );
    }

    #[test]
    fn test_read_skill_description_returns_none_for_missing_frontmatter() {
        struct NoFrontmatterFs;
        impl crate::hooks::FileSystemPort for NoFrontmatterFs {
            fn home_dir(&self) -> Option<std::path::PathBuf> {
                Some(std::path::PathBuf::from("/mock/home"))
            }
            fn read_to_string(&self, _: &std::path::Path) -> anyhow::Result<String> {
                Ok("# Just a heading\nNo frontmatter here.\n".to_string())
            }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
                Ok(vec![])
            }
            fn exists(&self, _: &std::path::Path) -> bool {
                true
            }
            fn is_dir(&self, _: &std::path::Path) -> bool {
                false
            }
            fn metadata(&self, _: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
                anyhow::bail!("nope")
            }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
        }
        assert_eq!(read_skill_description(&NoFrontmatterFs, "foo"), None);
    }

    #[test]
    fn test_is_valid_skill_rejects_garbage() {
        // A skill name with path traversal should not resolve. StubFs.exists
        // returns false for everything, so even a real skill name can't pass —
        // good: this test only verifies the path-traversal/nonexistent-name
        // cases don't blow up.
        assert!(!is_valid_skill(&StubFs, "../../../etc/passwd"));
        assert!(!is_valid_skill(&StubFs, "nonexistent-skill-xyz"));
    }

    #[tokio::test]
    async fn test_process_no_classifier_returns_no_match() {
        let input = HookInput {
            prompt: Some("run the tests".to_string()),
            ..Default::default()
        };
        let output = process(&input, None, &StubFs).await;
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
        let output = process(&input, None, &StubFs).await;
        assert!(output.hook_specific_output.is_none());
    }

    #[tokio::test]
    async fn test_process_no_prompt_returns_allow() {
        let input = HookInput::default();
        let output = process(&input, None, &StubFs).await;
        assert!(output.hook_specific_output.is_none());
    }
}
