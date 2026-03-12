//! Phase Gate Hook
//!
//! Blocks tool calls when skill phases are skipped.
//! Uses the workflow state machine to determine if a tool should be blocked.
//!
//! Enhanced features (ported from Node.js phase-gate.js):
//! - Tracks Read() calls on phase files via SessionState
//! - Formatted block messages with visual boxes
//! - Post-merge skip detection (review done but qa-handoff not loaded)
//! - Allows tools within 1 phase gap (mid-phase), blocks at 2+ gap

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;
use std::collections::HashMap;

/// Extracted phase file info from a Read() tool_input path.
#[derive(Debug, Clone)]
struct PhaseFileInfo {
    /// The phase filename (e.g., "claim.md")
    file: String,
    /// The skill name derived from the path (e.g., "linear")
    skill: String,
    /// Whether the file passed canonical path validation (exists on disk
    /// and resolves to within ~/.claude/skills/). Untrusted files are
    /// recorded for tracking but do NOT advance workflow state.
    trusted: bool,
}

/// Extract the phase file name AND skill name from a Read() tool_input path.
/// Matches paths like `~/.claude/skills/linear/phases/claim.md`
/// or `C:\Users\...\.claude\skills\linear\phases\claim.md`.
///
/// Returns `Some(PhaseFileInfo)` if the path is a valid phase file.
/// Validates:
/// - Path components match `skills/{name}/phases/{file}.md` pattern
/// - No `ParentDir` (`..`) components (checked via Path::components())
/// - Skill name and file name contain only safe ASCII characters
/// - Symlinks resolve to a path still under `~/.claude/skills/` (PathBuf API)
/// - `trusted` flag indicates whether canonical validation passed
fn extract_phase_file(tool_input: &serde_json::Value) -> Option<PhaseFileInfo> {
    use std::path::{Component, Path};

    // tool_input for Read is { "file_path": "..." }
    let path = tool_input
        .get("file_path")
        .and_then(|v| v.as_str())?;

    // Parse into path components — this handles both `/` and `\` separators
    // and gives us semantic components (Normal, ParentDir, RootDir, etc.)
    let file_path = Path::new(path);
    let components: Vec<Component> = file_path.components().collect();

    // Reject any ParentDir (..) components — checked structurally, not as substring.
    // This avoids both false positives (filenames containing "..") and false negatives
    // (edge cases where string matching diverges from OS path resolution).
    if components.iter().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }

    // Find the `skills` component and extract the pattern:
    //   skills / {skill_name} / phases / {phase_file}.md
    // Match as full path components, not substring — prevents matching
    // paths like `/foo/myskills/linear/phases/claim.md` or `skills_evil/...`.
    let skills_pos = components.iter().position(|c| {
        matches!(c, Component::Normal(s) if *s == std::ffi::OsStr::new("skills"))
    })?;

    // Need exactly 3 more components after "skills": {name}, "phases", {file}.md
    // And nothing after the .md file.
    if skills_pos + 4 != components.len() {
        return None;
    }

    let skill_name = components[skills_pos + 1]
        .as_os_str()
        .to_str()?;
    let phases_component = components[skills_pos + 2]
        .as_os_str()
        .to_str()?;
    let phase_file = components[skills_pos + 3]
        .as_os_str()
        .to_str()?;

    // Verify the "phases" directory component
    if phases_component != "phases" {
        return None;
    }

    // Must be a .md file
    if !phase_file.ends_with(".md") {
        return None;
    }

    // Validate names: ASCII alphanumeric + hyphens + underscores + dots only
    if !is_safe_name(skill_name) || !is_safe_name(phase_file) {
        return None;
    }

    // Symlink/canonical path resolution — eliminates TOCTOU by calling
    // canonicalize() directly without a prior exists() check.
    // canonicalize() returns Err if the file doesn't exist, which we handle.
    let canonical_result = file_path.canonicalize();

    // Track whether the file passed canonical validation.
    // Files that don't exist on disk are still extracted (for phase tracking)
    // but marked untrusted — the caller should not advance workflow state.
    let trusted = match &canonical_result {
        Ok(canonical) => {
            // Use PathBuf::starts_with() — component-aware, not string prefix.
            // This prevents sibling-directory tricks like `skills_evil/` matching
            // a string prefix of `skills`.
            let skills_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("skills");
            let skills_canonical = skills_dir
                .canonicalize()
                .unwrap_or(skills_dir);

            if !canonical.starts_with(&skills_canonical) {
                eprintln!(
                    "[sentinel] SECURITY: Phase file '{}' resolves to '{}' \
                     which is outside ~/.claude/skills/. Rejecting symlink escape.",
                    path,
                    canonical.display()
                );
                return None;
            }
            true
        }
        Err(_) => {
            // File doesn't exist on disk — textual validation passed above.
            // Mark as untrusted so workflow state is NOT advanced.
            false
        }
    };

    Some(PhaseFileInfo {
        file: phase_file.to_string(),
        skill: skill_name.to_string(),
        trusted,
    })
}

/// Validate a path segment contains only safe ASCII characters.
/// Allows: a-z, A-Z, 0-9, hyphens, underscores, dots (for .md extension).
///
/// Explicitly rejects ALL non-ASCII characters, including Unicode confusables
/// (e.g., Cyrillic 'а' U+0430 vs Latin 'a' U+0061) that could bypass
/// skill name matching via homoglyph attacks.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.is_ascii()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Process a phase-gate hook event (PreToolUse)
///
/// This function handles two responsibilities:
/// 1. Track Read() calls on phase files (recording them in state)
/// 2. Gate non-safe tool calls based on workflow phase progress
pub fn process(
    input: &HookInput,
    state: &mut SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
) -> HookOutput {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return HookOutput::allow(),
    };

    // Track ALL tool calls for phase-skip detection
    state.record_tool_call();

    // If this is a Read() call, check if it's reading a phase file
    if tool_name == "Read" {
        if let Some(ref tool_input) = input.tool_input {
            if let Some(info) = extract_phase_file(tool_input) {
                state.record_phase_read(&info.file);

                // Only advance workflow if the phase file is trusted (exists on
                // disk and canonicalizes to within ~/.claude/skills/).
                // Non-existent files are still recorded for tracking (phases_read)
                // but cannot advance state — prevents phantom phase completion
                // via crafted paths to files that don't exist.
                if !info.trusted {
                    return HookOutput::allow();
                }

                // Auto-advance workflow when phase file is read.
                // Reading the phase file = proof of engagement under hard gate.
                //
                // FIX: Derive skill from the path (info.skill), not active_skill.
                // This prevents misattribution when multiple skills are in play.
                // Fall back to active_skill only if the path-derived skill has
                // no workflow definition in the config.
                let skill_to_advance = if workflows.contains_key(&info.skill) {
                    Some(info.skill.clone())
                } else if let Some(ref active) = state.active_skill {
                    if workflows.contains_key(active.as_str()) {
                        // Fallback: path-derived skill not in workflows, using active_skill.
                        // This is a potential misconfiguration — the phase file path
                        // references a skill that has no workflow definition.
                        eprintln!(
                            "[sentinel] WARNING: Phase file path references skill '{}' \
                             which has no workflow definition. Falling back to active_skill '{}'. \
                             This may indicate a misconfigured skill or stale phase file.",
                            info.skill, active
                        );
                        Some(active.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(skill_name) = skill_to_advance {
                    // FIX: Use strip_suffix instead of trim_end_matches.
                    // trim_end_matches removes repeated char patterns, not the suffix.
                    // e.g., "add.md" with trim_end_matches(".md") would strip "d" too.
                    let phase_id = info.file.strip_suffix(".md").unwrap_or(&info.file);

                    // Validate phase_id against known workflow phases before advancing.
                    // Only advance if this is a recognized phase, preventing
                    // arbitrary state manipulation via crafted filenames.
                    let is_known_phase = workflows
                        .get(&skill_name)
                        .map(|w| w.phases.iter().any(|p| p.id == phase_id))
                        .unwrap_or(false);

                    if is_known_phase {
                        // Ensure workflow state exists for this skill
                        if !state.workflows.contains_key(&skill_name) {
                            state.workflows.insert(
                                skill_name.clone(),
                                sentinel_domain::workflow::WorkflowState::new(
                                    &skill_name,
                                    &state.session_id,
                                ),
                            );
                        }
                        if let Some(wf) = state.workflows.get_mut(&skill_name) {
                            wf.advance(phase_id);
                        }
                    }
                }
            }
        }
        // Read calls always pass through (they're safe tools)
        return HookOutput::allow();
    }

    // Delegate to gate evaluation for blocking decisions
    let result = crate::gate::evaluate(state, workflows, input);
    match result {
        crate::gate::GateDecision::Allow => {
            // Additional post-merge skip detection:
            // If review.md is read but qa-handoff.md is not, and we're past
            // the review phase, block non-safe tools
            if let Some(block) = check_post_merge_skip(state, workflows, tool_name) {
                return block;
            }
            HookOutput::allow()
        }
        crate::gate::GateDecision::Block {
            reason,
            next_phase,
            next_phase_file,
        } => {
            let skill = state.active_skill.as_deref().unwrap_or("unknown");
            let completed = state
                .active_workflow()
                .map(|w| w.completed_phases.len())
                .unwrap_or(0);
            let total = workflows
                .get(skill)
                .map(|w| w.phases.iter().filter(|p| p.required).count())
                .unwrap_or(0);

            let message = format_block_box(
                skill,
                &reason,
                &next_phase,
                &next_phase_file,
                completed,
                total,
            );
            HookOutput::block(message)
        }
    }
}

/// Check for post-merge phase skip:
/// If review.md has been read (or review phase completed) but qa-handoff.md
/// has not been read, block non-safe tools. This catches the case where
/// Claude tries to skip QA after code review.
fn check_post_merge_skip(
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    tool_name: &str,
) -> Option<HookOutput> {
    // Only check safe-tool-exempt tools
    let safe_tools = [
        "Read", "Glob", "Grep", "WebSearch", "WebFetch", "Task",
        "AskUserQuestion", "EnterPlanMode", "ExitPlanMode", "TaskCreate",
        "TaskUpdate", "TaskList", "TaskGet", "Skill", "ToolSearch",
        "Agent",
    ];
    if safe_tools.contains(&tool_name) {
        return None;
    }

    let skill = state.active_skill.as_ref()?;
    let workflow = workflows.get(skill)?;

    // Check if this workflow has both review and qa-handoff phases
    let has_review = workflow.phases.iter().any(|p| p.id == "review");
    let has_qa = workflow.phases.iter().any(|p| p.id == "qa-handoff");
    if !has_review || !has_qa {
        return None;
    }

    // Check: review.md loaded but qa-handoff.md NOT loaded
    let review_read = state.phases_read.contains(&"review.md".to_string());
    let qa_read = state.phases_read.contains(&"qa-handoff.md".to_string());

    // Also check completed phases (from submit_phase_complete)
    let review_complete = state
        .active_workflow()
        .map(|w| w.is_phase_complete("review"))
        .unwrap_or(false);

    if (review_read || review_complete) && !qa_read {
        let message = format!(
            "\
+============================================================+
|  BLOCKED: Post-Merge Phase Skip Detected                   |
+============================================================+
|  review.md has been loaded but qa-handoff.md has NOT.       |
|                                                            |
|  After code review, you MUST load the QA handoff phase     |
|  before making any further tool calls.                     |
|                                                            |
|  MANDATORY: Read(\"~/.claude/skills/{}/phases/qa-handoff.md\")|
+============================================================+",
            skill
        );
        return Some(HookOutput::block(message));
    }

    None
}

/// Format a visually prominent block message box
fn format_block_box(
    skill: &str,
    reason: &str,
    next_phase: &str,
    next_phase_file: &str,
    completed: usize,
    total: usize,
) -> String {
    format!(
        "\
+============================================================+
|  BLOCKED: Phase Gate Violation                             |
+============================================================+
|  Skill: {skill:<50}|
|  Progress: {completed}/{total} required phases completed              |
|                                                            |
|  Reason: {reason:<49}|
|                                                            |
|  Next required phase: {next_phase:<35}|
|  Read(\"~/.claude/skills/{skill}/phases/{next_phase_file}\")    |
+============================================================+"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::WorkflowPhase;

    fn test_workflow() -> SkillWorkflow {
        SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![
                WorkflowPhase {
                    id: "claim".to_string(),
                    file: "claim.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Claim the issue".to_string(),
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Fetch details".to_string(),
                },
                WorkflowPhase {
                    id: "review".to_string(),
                    file: "review.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "Code review".to_string(),
                },
                WorkflowPhase {
                    id: "qa-handoff".to_string(),
                    file: "qa-handoff.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "QA handoff".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_allows_when_no_active_skill() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_safe_tools() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Glob".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_phase_files_on_disk() {
        // Phase gate skips enforcement when phase files don't exist on disk.
        // Use a fake skill name that definitely has no phase files.
        let fake_workflow = SkillWorkflow {
            skill: "nonexistent-test-skill".to_string(),
            phases: vec![WorkflowPhase {
                id: "setup".to_string(),
                file: "setup.md".to_string(),
                required: true,
                judge: JudgeModel::Sonnet,
                description: "Setup".to_string(),
            }],
        };
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("nonexistent-test-skill");
        let mut workflows = HashMap::new();
        workflows.insert("nonexistent-test-skill".to_string(), fake_workflow);

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        // No phase files on disk → gate allows
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_when_phase_files_exist() {
        // Phase gate enforces when phase files exist on disk.
        // Uses "linear" which has real phase files at ~/.claude/skills/linear/phases/
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);

        // Check if linear phase files exist on this machine
        let claim_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".claude/skills/linear/phases/claim.md");
        if claim_path.exists() {
            // Phase files exist → gate blocks
            assert_eq!(output.blocked, Some(true));
            assert!(output.reason.as_ref().unwrap().contains("BLOCKED"));
            assert!(output.reason.as_ref().unwrap().contains("claim"));
        } else {
            // No phase files → gate allows (CI/other machines)
            assert!(output.blocked.is_none());
        }
    }

    #[test]
    fn test_read_on_phase_file_records_and_advances() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        // Use a real absolute path so canonicalize() succeeds (trusted = true)
        let claim_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/claim.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": claim_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        // Read should always be allowed
        assert!(output.blocked.is_none());
        // Should record the phase read
        assert!(state.phases_read.contains(&"claim.md".to_string()));

        if claim_path.exists() {
            // File exists on disk → trusted → workflow advances
            let wf_state = state.workflows.get("linear").unwrap();
            assert!(wf_state.is_phase_complete("claim"));
        } else {
            // File doesn't exist (CI/other machines) → untrusted → no advance
            assert!(state.workflows.get("linear").is_none()
                || !state.workflows.get("linear").unwrap().is_phase_complete("claim"));
        }
    }

    #[test]
    fn test_read_derives_skill_from_path() {
        // Even if active_skill is different, the phase advance should use
        // the skill derived from the path, not active_skill.
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("some-other-skill");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let claim_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/claim.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": claim_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        assert!(state.phases_read.contains(&"claim.md".to_string()));

        if claim_path.exists() {
            // File exists → trusted → should advance "linear" (from path), not "some-other-skill"
            let wf_state = state.workflows.get("linear").unwrap();
            assert!(wf_state.is_phase_complete("claim"));
        }
        // If file doesn't exist → untrusted → no advance (still OK, phases_read recorded)
    }

    #[test]
    fn test_read_rejects_unknown_phase_id() {
        // Reading a .md file that isn't a known phase should NOT advance workflow
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let evil_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/evil-phase.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": evil_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        // File is recorded as read (for tracking), but workflow should NOT advance
        // (evil-phase.md doesn't exist on disk → untrusted, AND not a known phase ID)
        assert!(state.phases_read.contains(&"evil-phase.md".to_string()));
        // No workflow state should be created for unknown phases
        assert!(
            state.workflows.get("linear").is_none()
                || !state.workflows.get("linear").unwrap().is_phase_complete("evil-phase")
        );
    }

    #[test]
    fn test_read_rejects_path_traversal() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let traversal_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/../../secrets/claim.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": traversal_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        // Path traversal should be rejected (ParentDir component detected) — no phase recorded
        assert!(state.phases_read.is_empty());
    }

    #[test]
    fn test_read_on_windows_path_records_phase() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "C:\\Users\\garys\\.claude\\skills\\linear\\phases\\fetch.md"
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        assert!(state.phases_read.contains(&"fetch.md".to_string()));
    }

    #[test]
    fn test_read_on_non_phase_file_ignored() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let skill_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/SKILL.md");

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": skill_path.to_string_lossy()
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        assert!(state.phases_read.is_empty());
    }

    #[test]
    fn test_post_merge_skip_blocks() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        // Mark review as read but not qa-handoff
        state.record_phase_read("claim.md");
        state.record_phase_read("fetch.md");
        state.record_phase_read("review.md");

        // Complete claim, fetch, review phases so gate doesn't block on those
        if let Some(wf) = state.workflows.get_mut("linear") {
            wf.advance("claim");
            wf.advance("fetch");
            wf.advance("review");
        }

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("Post-Merge"));
        assert!(output.reason.as_ref().unwrap().contains("qa-handoff.md"));
    }

    #[test]
    fn test_post_merge_skip_allows_when_qa_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("claim.md");
        state.record_phase_read("fetch.md");
        state.record_phase_read("review.md");
        state.record_phase_read("qa-handoff.md");

        // Complete all phases
        if let Some(wf) = state.workflows.get_mut("linear") {
            wf.advance("claim");
            wf.advance("fetch");
            wf.advance("review");
            wf.advance("qa-handoff");
        }

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        // Should be allowed — all phases read
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_untrusted_file_does_not_advance_workflow() {
        // A phase file path that doesn't exist on disk should be recorded
        // in phases_read but should NOT advance workflow state (untrusted).
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        // Use a path with a nonexistent parent dir so the file definitely doesn't exist
        let fake_path = dirs::home_dir()
            .unwrap()
            .join(".claude/skills/linear/phases/claim.md");

        // Only run this test if the file does NOT exist (CI environments)
        // On dev machines where the file exists, skip this variant
        if !fake_path.exists() {
            let input = HookInput {
                tool_name: Some("Read".to_string()),
                tool_input: Some(serde_json::json!({
                    "file_path": fake_path.to_string_lossy()
                })),
                ..Default::default()
            };
            let output = process(&input, &mut state, &workflows);
            assert!(output.blocked.is_none());
            // File recorded for tracking
            assert!(state.phases_read.contains(&"claim.md".to_string()));
            // But workflow NOT advanced (untrusted — file doesn't exist)
            assert!(
                state.workflows.get("linear").is_none()
                    || !state.workflows.get("linear").unwrap().is_phase_complete("claim")
            );
        }
    }

    #[test]
    fn test_tool_call_counter_increments() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        process(&input, &mut state, &workflows);
        process(&input, &mut state, &workflows);
        assert_eq!(state.tool_calls, 2);
    }
}
