//! Skill Invocation Gate — enforces that detected skills are actually invoked.
//!
//! When `skill_router` detects a skill, it injects a `MANDATORY:
//! Skill(skill: ...)` message asking Claude to invoke the skill before
//! continuing. That instruction is advisory — Claude can ignore it and call
//! other tools without ever invoking the skill. This hook makes the rule
//! binding:
//!
//! 1. **`PreToolUse`** — if a pending-skill state file exists for the session,
//!    block any tool call outside the read-only allowlist with a clear "load
//!    the skill first" message. The allowlist includes Read/Glob/Grep so
//!    Claude can inspect files while recovering, plus `Skill` itself so the
//!    invocation that satisfies the gate isn't blocked by it.
//!
//! 2. **`PostToolUse`** — when the `Skill` tool fires with a name matching the
//!    pending skill, clear the state. State also auto-clears after a 5-minute
//!    TTL so a skill the user explicitly skipped doesn't deadlock the session.

use chrono::Utc;
use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};

use super::{skill_router::PendingSkillState, FileSystemPort, HookContext};

/// Read-only / progress-toward-skill-load tools that should never be blocked.
/// These either don't change state (Read/Glob/Grep/LSP/Web*) or are how the
/// model satisfies the gate (Skill/TaskCreate/TaskUpdate/sequentialthinking).
const ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LSP",
    "WebSearch",
    "WebFetch",
    "ToolSearch",
    "Skill",
    "TaskList",
    "TaskGet",
    "TaskCreate",
    "TaskUpdate",
    "TaskOutput",
    "mcp__sequential-thinking__sequentialthinking",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillInvocationDecision {
    Allow,
    Block,
}

#[derive(Debug, Clone)]
pub struct SkillInvocationEvaluation {
    pub tool: Option<String>,
    pub session_id: Option<String>,
    pub subagent_call: bool,
    pub session_id_present: bool,
    pub pending_skill_present: bool,
    pub pending_skill_stale: bool,
    pub pending_state_session_matches: bool,
    pub allowed_tool: bool,
    pub skill: Option<String>,
    pub skill_present: bool,
    pub skill_path_present: bool,
    pub detected_at_present: bool,
    pub should_block: bool,
    pub decision: SkillInvocationDecision,
}

impl SkillInvocationEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        !self.subagent_call
            && self.session_id_present
            && self.pending_skill_present
            && !self.pending_skill_stale
    }
}

/// Pending state has a 5-minute TTL. After that, clear it and allow tools
/// through — prevents a skill the user has explicitly moved on from from
/// permanently deadlocking the session.
const PENDING_TTL_SECS: i64 = 300;

fn load_pending_state(fs: &dyn FileSystemPort, session_id: &str) -> Option<PendingSkillState> {
    let path = super::skill_router::pending_skill_state_path(fs, session_id)?;
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn clear_pending_state(fs: &dyn FileSystemPort, session_id: &str) {
    // Delegate to the router's helper, which removes the file via
    // `remove_file`. (The old implementation overwrote the file with empty
    // bytes because it assumed FileSystemPort had no remove(); it does.)
    super::skill_router::clear_pending_skill_state(fs, session_id);
}

/// True when `detected_at` is older than the TTL.
fn is_stale(detected_at: &str) -> bool {
    let parsed = chrono::DateTime::parse_from_rfc3339(detected_at);
    match parsed {
        Ok(dt) => Utc::now().signed_duration_since(dt).num_seconds() > PENDING_TTL_SECS,
        // Unparseable timestamp → treat as stale; better to fail-open than
        // deadlock on corrupt state.
        Err(_) => true,
    }
}

/// True when this tool call originates from inside a subagent rather than the
/// main session. Claude Code stamps `agent_id` (and usually `agent_type`) on
/// every hook payload that fires from a spawned agent's context; the main
/// session leaves both unset. We treat either being present as "subagent".
///
/// The pending-skill state file is keyed by `session_id`, which a subagent
/// shares with its parent. Without this check the gate would block a
/// subagent's tools based on a skill the *main* session was asked to invoke —
/// a skill the subagent was never told about and has no way to satisfy.
fn is_subagent(input: &HookInput) -> bool {
    let nonempty = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.is_empty());
    nonempty(&input.agent_id) || nonempty(&input.agent_type)
}

/// `PreToolUse` handler — block when there's a pending skill and the tool
/// isn't on the allowlist.
pub fn process_pretool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let evaluation = evaluate_pretool(input, ctx);
    apply_pretool_side_effects(&evaluation, ctx);
    output_from_pretool_evaluation(&evaluation)
}

#[must_use]
pub fn evaluate_pretool(input: &HookInput, ctx: &HookContext<'_>) -> SkillInvocationEvaluation {
    let tool = input.tool_name.clone();
    let tool_name = input.tool_name.as_deref().unwrap_or("");
    let session_id = input.session_id.clone();
    let subagent_call = is_subagent(input);
    let allowed_tool = ALLOWED_TOOLS.contains(&tool_name);

    // Never gate subagent/teammate tool calls. The pending-skill marker is a
    // main-session concept: it records that the user's message routed to a
    // skill the *main* session must invoke. A subagent shares the parent's
    // `session_id` but runs in its own context with its own instructions, so
    // gating it on the parent's pending skill is always wrong (and the
    // subagent can't clear the marker without derailing its own task).
    if subagent_call {
        return SkillInvocationEvaluation {
            tool,
            session_id,
            subagent_call,
            session_id_present: input.session_id.as_deref().is_some_and(|s| !s.is_empty()),
            pending_skill_present: false,
            pending_skill_stale: false,
            pending_state_session_matches: false,
            allowed_tool,
            skill: None,
            skill_present: false,
            skill_path_present: false,
            detected_at_present: false,
            should_block: false,
            decision: SkillInvocationDecision::Allow,
        };
    }

    let session_id = match input.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return SkillInvocationEvaluation {
                tool,
                session_id,
                subagent_call,
                session_id_present: false,
                pending_skill_present: false,
                pending_skill_stale: false,
                pending_state_session_matches: false,
                allowed_tool,
                skill: None,
                skill_present: false,
                skill_path_present: false,
                detected_at_present: false,
                should_block: false,
                decision: SkillInvocationDecision::Allow,
            };
        }
    };

    let state = match load_pending_state(ctx.fs, session_id) {
        Some(s) => s,
        None => {
            return SkillInvocationEvaluation {
                tool,
                session_id: Some(session_id.to_string()),
                subagent_call,
                session_id_present: true,
                pending_skill_present: false,
                pending_skill_stale: false,
                pending_state_session_matches: false,
                allowed_tool,
                skill: None,
                skill_present: false,
                skill_path_present: false,
                detected_at_present: false,
                should_block: false,
                decision: SkillInvocationDecision::Allow,
            };
        }
    };

    // Auto-clear stale state so the gate is self-healing.
    let pending_skill_stale = is_stale(&state.detected_at);
    let skill_present = !state.skill.trim().is_empty();
    let skill_path_present = !state.skill_path.trim().is_empty();
    let detected_at_present = !state.detected_at.trim().is_empty();
    let pending_state_session_matches = state.session_id == session_id;
    let should_block = !pending_skill_stale && !allowed_tool;
    SkillInvocationEvaluation {
        tool,
        session_id: Some(session_id.to_string()),
        subagent_call,
        session_id_present: true,
        pending_skill_present: true,
        pending_skill_stale,
        pending_state_session_matches,
        allowed_tool,
        skill: Some(state.skill),
        skill_present,
        skill_path_present,
        detected_at_present,
        should_block,
        decision: if should_block {
            SkillInvocationDecision::Block
        } else {
            SkillInvocationDecision::Allow
        },
    }
}

pub fn apply_pretool_side_effects(evaluation: &SkillInvocationEvaluation, ctx: &HookContext<'_>) {
    if evaluation.pending_skill_present && evaluation.pending_skill_stale {
        if let Some(session_id) = evaluation.session_id.as_deref() {
            clear_pending_state(ctx.fs, session_id);
        }
    }
}

#[must_use]
pub fn output_from_pretool_evaluation(evaluation: &SkillInvocationEvaluation) -> HookOutput {
    if !matches!(evaluation.decision, SkillInvocationDecision::Block) {
        return HookOutput::allow();
    }

    let skill = evaluation.skill.as_deref().unwrap_or("");
    let tool_name = evaluation.tool.as_deref().unwrap_or("");

    let envelope = HookEnvelope::block(
        "Skill Gate",
        format!(
            "Skill `{}` was detected but not invoked. Call `Skill(skill: \"{0}\")` \
             before using `{}`. \
             (Read-only tools and TaskCreate/TaskUpdate are allowed.)",
            skill, tool_name,
        ),
    );
    HookOutput::block(envelope.render())
}

/// `PostToolUse` handler — clear the pending state once the `Skill` tool has
/// been invoked with a matching skill name.
pub fn process_posttool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = match input.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return HookOutput::allow(),
    };

    let state = match load_pending_state(ctx.fs, session_id) {
        Some(s) => s,
        None => return HookOutput::allow(),
    };

    let tool_name = input.tool_name.as_deref().unwrap_or("");
    let cleared = tool_name == "Skill" && skill_arg_matches(input, &state.skill);

    if cleared {
        clear_pending_state(ctx.fs, session_id);
        // Record that this skill has been satisfied for the session so the
        // skill_router will NOT re-arm the gate the next time it detects the
        // same skill on a subsequent turn (e.g. cron-injected prompts that
        // mention the skill keyword, or multi-turn conversations). Without
        // this, re-detection unconditionally overwrites the pending marker
        // with a fresh timestamp, causing the gate to block on every turn.
        super::skill_router::mark_skill_satisfied(ctx.fs, session_id, &state.skill);
    }
    HookOutput::allow()
}

/// True when the Skill tool's `skill` arg matches the pending name.
fn skill_arg_matches(input: &HookInput, expected_skill: &str) -> bool {
    input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("skill"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == expected_skill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Real-filesystem adapter scoped to a temp directory. Used for tests that
    /// exercise the full pending + satisfied state round-trip on disk, which the
    /// in-memory `StubFs` cannot support (it always returns Err for reads).
    struct TempDirFs {
        home: PathBuf,
    }

    impl crate::hooks::FileSystemPort for TempDirFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            f.write_all(c)?;
            Ok(())
        }
        fn remove_file(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if p.exists() {
                std::fs::remove_file(p)?;
            }
            Ok(())
        }
    }

    #[test]
    fn test_is_stale_true_for_old_timestamp() {
        let old = (Utc::now() - chrono::Duration::seconds(PENDING_TTL_SECS + 60)).to_rfc3339();
        assert!(is_stale(&old));
    }

    #[test]
    fn test_is_stale_false_for_recent() {
        let recent = Utc::now().to_rfc3339();
        assert!(!is_stale(&recent));
    }

    #[test]
    fn test_is_stale_true_for_garbage() {
        // Unparseable → treat as stale (fail-open).
        assert!(is_stale("not-a-real-timestamp"));
    }

    #[test]
    fn test_skill_arg_matches() {
        let input = HookInput {
            tool_input: Some(serde_json::json!({"skill": "linear"})),
            ..Default::default()
        };
        assert!(skill_arg_matches(&input, "linear"));
        assert!(!skill_arg_matches(&input, "memory"));
    }

    #[test]
    fn test_skill_arg_matches_handles_missing_input() {
        let input = HookInput::default();
        assert!(!skill_arg_matches(&input, "linear"));
    }

    #[test]
    fn test_pretool_allows_when_no_pending_state() {
        let input = HookInput {
            session_id: Some("test".to_string()),
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_pretool(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_pretool_allows_when_no_session_id() {
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_pretool(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allowed_tools_include_skill_and_read() {
        // Sanity: Skill satisfies the gate and Read remains allowed as a
        // harmless inspection tool while recovering from the block.
        assert!(ALLOWED_TOOLS.contains(&"Skill"));
        assert!(ALLOWED_TOOLS.contains(&"Read"));
    }

    #[test]
    fn test_is_subagent_detects_agent_id() {
        let input = HookInput {
            agent_id: Some("a3f2-deadbeef".to_string()),
            ..Default::default()
        };
        assert!(is_subagent(&input));
    }

    #[test]
    fn test_is_subagent_detects_agent_type() {
        let input = HookInput {
            agent_type: Some("code-reviewer".to_string()),
            ..Default::default()
        };
        assert!(is_subagent(&input));
    }

    #[test]
    fn test_is_subagent_false_for_main_session() {
        // Main-session payloads leave both fields unset (or empty).
        let input = HookInput {
            agent_id: Some(String::new()),
            agent_type: None,
            ..Default::default()
        };
        assert!(!is_subagent(&input));
        assert!(!is_subagent(&HookInput::default()));
    }

    #[test]
    fn test_pretool_never_blocks_subagent_tool_calls() {
        // The core fix: even with a session_id whose pending-skill marker would
        // otherwise gate a non-allowlisted tool, a subagent call (agent_id set)
        // must pass through. We don't need pending state on disk to prove this —
        // is_subagent short-circuits before state is ever loaded — but we use a
        // real non-allowlisted tool name to make the intent explicit.
        let input = HookInput {
            session_id: Some("shared-with-parent".to_string()),
            tool_name: Some("Bash".to_string()),
            agent_id: Some("a3f2-deadbeef".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_pretool(&input, &ctx);
        assert!(
            output.blocked.is_none(),
            "subagent Bash call must not be gated on the parent's pending skill",
        );
    }

    /// Regression test for the per-turn re-blocking loop (SEN-skill-gate-refire).
    ///
    /// Sequence:
    ///   Turn 1: skill_router writes pending marker for "linear".
    ///   Turn 1: Claude invokes Skill("linear") → process_posttool clears pending
    ///           AND marks "linear" as satisfied for the session.
    ///   Turn 2: skill_router re-detects "linear" → build_match_output sees
    ///           "linear" is already satisfied → does NOT write pending marker.
    ///   Turn 2: gate MUST allow Bash (no pending state to block on).
    #[test]
    fn test_gate_allows_bash_after_skill_invoked_even_on_redetection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fs = TempDirFs {
            home: tmp.path().to_path_buf(),
        };
        let session_id = "test-session-refire-regression";
        let skill = "linear";
        let skill_path = format!("~/.claude/skills/{skill}/SKILL.md");

        // — Turn 1a: router writes pending marker —
        super::super::skill_router::write_pending_skill_state(&fs, skill, &skill_path, session_id);

        // Gate should block Bash at this point (pending marker exists).
        {
            let git = Box::leak(Box::new(crate::hooks::test_support::StubGit));
            let process = Box::leak(Box::new(crate::hooks::test_support::StubProcess));
            let memory_mcp = Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
            let env = Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
            let fs_ref: &TempDirFs = &fs;
            // SAFETY: we hold tmp alive for the whole test scope, so the
            // reference is valid. The Box::leak for other ports is fine since
            // tests are single-process and these tiny structs have no Drop.
            let ctx = crate::hooks::HookContext {
                git,
                vector_store: None,
                fs: fs_ref,
                process,
                llm: None,
                memory_mcp,
                env,
                linear_lookup: None,
            };
            let pretool_input = HookInput {
                session_id: Some(session_id.to_string()),
                tool_name: Some("Bash".to_string()),
                ..Default::default()
            };
            let out = process_pretool(&pretool_input, &ctx);
            assert!(
                out.blocked.is_some(),
                "gate must block Bash when pending marker exists (pre-condition check)"
            );
        }

        // — Turn 1b: Claude calls Skill("linear") → posttool clears pending
        //            and records the skill as satisfied —
        {
            let git = Box::leak(Box::new(crate::hooks::test_support::StubGit));
            let process = Box::leak(Box::new(crate::hooks::test_support::StubProcess));
            let memory_mcp = Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
            let env = Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
            let fs_ref: &TempDirFs = &fs;
            let ctx = crate::hooks::HookContext {
                git,
                vector_store: None,
                fs: fs_ref,
                process,
                llm: None,
                memory_mcp,
                env,
                linear_lookup: None,
            };
            let posttool_input = HookInput {
                session_id: Some(session_id.to_string()),
                tool_name: Some("Skill".to_string()),
                tool_input: Some(serde_json::json!({"skill": skill})),
                ..Default::default()
            };
            let out = process_posttool(&posttool_input, &ctx);
            assert!(out.blocked.is_none(), "posttool must not block");
        }

        // — Turn 2: router re-detects "linear" and would normally overwrite
        //   the pending marker. With the fix, it must skip writing because
        //   the skill is now in the satisfied set. —
        super::super::skill_router::write_pending_skill_state_if_not_satisfied(
            &fs,
            skill,
            &skill_path,
            session_id,
        );

        // Gate must allow Bash — no pending state should exist.
        {
            let git = Box::leak(Box::new(crate::hooks::test_support::StubGit));
            let process = Box::leak(Box::new(crate::hooks::test_support::StubProcess));
            let memory_mcp = Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
            let env = Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
            let fs_ref: &TempDirFs = &fs;
            let ctx = crate::hooks::HookContext {
                git,
                vector_store: None,
                fs: fs_ref,
                process,
                llm: None,
                memory_mcp,
                env,
                linear_lookup: None,
            };
            let pretool_input = HookInput {
                session_id: Some(session_id.to_string()),
                tool_name: Some("Bash".to_string()),
                ..Default::default()
            };
            let out = process_pretool(&pretool_input, &ctx);
            assert!(
                out.blocked.is_none(),
                "gate MUST allow Bash after skill was invoked, even when router re-detects the same skill on a later turn"
            );
        }
    }

    #[test]
    fn test_reading_skill_md_does_not_satisfy_pending_skill() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fs = TempDirFs {
            home: tmp.path().to_path_buf(),
        };
        let session_id = "test-session-read-no-satisfy";
        let skill = "linear";
        let skill_path = format!("~/.claude/skills/{skill}/SKILL.md");
        super::super::skill_router::write_pending_skill_state(&fs, skill, &skill_path, session_id);

        let git = Box::leak(Box::new(crate::hooks::test_support::StubGit));
        let process = Box::leak(Box::new(crate::hooks::test_support::StubProcess));
        let memory_mcp = Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
        let env = Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
        let fs_ref: &TempDirFs = &fs;
        let ctx = crate::hooks::HookContext {
            git,
            vector_store: None,
            fs: fs_ref,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        };

        let read_input = HookInput {
            session_id: Some(session_id.to_string()),
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({"file_path": skill_path})),
            ..Default::default()
        };
        let read_out = process_posttool(&read_input, &ctx);
        assert!(read_out.blocked.is_none(), "posttool must not block");

        let bash_input = HookInput {
            session_id: Some(session_id.to_string()),
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let bash_out = process_pretool(&bash_input, &ctx);
        assert!(
            bash_out.blocked.is_some(),
            "Read(SKILL.md) must not clear the pending skill; only Skill(skill: ...) satisfies the gate"
        );
    }

    /// Sanity: a genuinely new skill (never invoked this session) still blocks.
    #[test]
    fn test_gate_still_blocks_newly_detected_never_invoked_skill() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fs = TempDirFs {
            home: tmp.path().to_path_buf(),
        };
        let session_id = "test-session-new-skill";
        let skill = "memory";
        let skill_path = format!("~/.claude/skills/{skill}/SKILL.md");

        // Mark a *different* skill as satisfied (simulates a prior invocation
        // of "linear" — should have no effect on blocking "memory").
        super::super::skill_router::mark_skill_satisfied(&fs, session_id, "linear");

        // Router writes pending marker for the new, never-invoked skill "memory".
        super::super::skill_router::write_pending_skill_state_if_not_satisfied(
            &fs,
            skill,
            &skill_path,
            session_id,
        );

        let git = Box::leak(Box::new(crate::hooks::test_support::StubGit));
        let process = Box::leak(Box::new(crate::hooks::test_support::StubProcess));
        let memory_mcp = Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
        let env = Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
        let fs_ref: &TempDirFs = &fs;
        let ctx = crate::hooks::HookContext {
            git,
            vector_store: None,
            fs: fs_ref,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        };
        let pretool_input = HookInput {
            session_id: Some(session_id.to_string()),
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let out = process_pretool(&pretool_input, &ctx);
        assert!(
            out.blocked.is_some(),
            "gate MUST still block a newly-detected skill that has never been invoked"
        );
    }
}
