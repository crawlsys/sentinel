//! `TaskCompleted` hook — verification gate for task completion
//!
//! When a task is being marked complete, reminds the teammate to verify
//! their work before marking it done. This is the team-level equivalent
//! of the `verification_gate` hook.

use std::fmt::Write as _;

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Extract a Linear issue ID from a task subject containing `@linear:{ID}`.
///
/// Returns `Some("PREFIX-123")` if found, `None` otherwise.
fn extract_linear_id(subject: &str) -> Option<&str> {
    let marker = "@linear:";
    let start = subject.find(marker)?;
    let after = &subject[start + marker.len()..];
    // Linear IDs are PREFIX-NUMBER (e.g. FIR-123, SYN-42)
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    let id = &after[..end];
    // Validate shape: at least one letter, a hyphen, at least one digit
    if let Some(hyphen) = id.find('-') {
        let prefix = &id[..hyphen];
        let number = &id[hyphen + 1..];
        if !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphanumeric())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
        {
            return Some(id);
        }
    }
    None
}

/// Concrete-artifact claims a task subject/description can assert.
///
/// Detected purely from the task text (subject + description). Each variant is
/// a *claim of completed work* whose truth we can try to corroborate against
/// the working tree before letting the teammate mark the task ✅.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ClaimSignals {
    /// Text claims a commit/push happened ("committed", "pushed", a sha, ✅).
    commit: bool,
    /// Text references a PR ("PR #N", "pull request", "opened/merged PR").
    pr: bool,
    /// Text claims a build/test outcome ("build clean", "tests pass", "N passed").
    build_test: bool,
}

impl ClaimSignals {
    fn any(self) -> bool {
        self.commit || self.pr || self.build_test
    }
}

/// Scan a task's subject + description for concrete-artifact claim signals.
///
/// Lowercased substring matching — deliberately cheap and heuristic. The goal
/// is to catch the obvious "plausible-but-false done" phrasings, not to parse
/// natural language. False negatives (missed claims) are fine; we only act on
/// a *mismatch*, so a missed claim just means no warning.
fn detect_claims(text: &str) -> ClaimSignals {
    let lower = text.to_lowercase();

    let commit = lower.contains("commit")        // commit / committed / commits
        || lower.contains("pushed")
        || lower.contains("push to")
        || contains_sha(&lower);

    let pr = lower.contains("pr #")
        || lower.contains("pull request")
        || lower.contains("opened pr")
        || lower.contains("merged pr")
        || lower.contains("pr merged")
        || lower.contains(" pr ")
        || lower.starts_with("pr ");

    let build_test = lower.contains("build clean")
        || lower.contains("build pass")
        || lower.contains("builds clean")
        || lower.contains("tests pass")
        || lower.contains("test pass")
        || lower.contains("tests passing")
        || lower.contains("all green")
        || lower.contains("passed")
        || lower.contains("passing");

    ClaimSignals {
        commit,
        pr,
        build_test,
    }
}

/// Heuristic: does the (already-lowercased) text contain a git-sha-looking
/// token — a standalone run of 7–40 hex chars with at least one digit? Used as
/// a weak commit signal. Requiring a digit avoids tripping on plain hex-only
/// words.
fn contains_sha(lower: &str) -> bool {
    lower.split(|c: char| !c.is_ascii_hexdigit()).any(|tok| {
        let n = tok.len();
        (7..=40).contains(&n)
            && tok.chars().all(|c| c.is_ascii_hexdigit())
            && tok.chars().any(|c| c.is_ascii_digit())
    })
}

/// Run the claim-vs-reality checks and return any mismatch warning lines.
///
/// FAIL OPEN: any git error is swallowed (treated as "can't verify" → no
/// warning). We only emit a hard warning on a *real, observed* mismatch; soft
/// "please confirm" notes are emitted for claims we cannot verify here.
///
/// `first_in_progress` is true when this is the first time the task entered
/// `in_progress` (suspiciously fast done — claimed complete on the first hop).
fn verify_claims(
    claims: ClaimSignals,
    ctx: &super::HookContext<'_>,
    cwd: &str,
    first_in_progress: bool,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();

    if !claims.any() {
        return warnings;
    }

    // --- Commit/push claim: the KEY high-value check ---
    // If the task says it committed/pushed but the working tree is still dirty,
    // that's a concrete contradiction worth surfacing.
    if claims.commit {
        // has_uncommitted_changes returns Err on git failure → fail open (false).
        let dirty = ctx.git.has_uncommitted_changes(cwd).unwrap_or(false);
        // Only treat as a mismatch when we can actually confirm a repo + HEAD.
        // No HEAD (unborn / not a repo) → can't verify → stay silent.
        let has_head = ctx.git.head_sha(cwd).is_some();
        if dirty && has_head {
            warnings.push(
                "claims a commit/push, but the working tree still has \
                 UNCOMMITTED changes — confirm the work was actually committed \
                 (run `git status`), not just edited"
                    .to_string(),
            );
        }
    }

    // --- PR claim: can't verify without gh → soft confirm note (only if claimed) ---
    if claims.pr {
        warnings.push(
            "references a PR (opened/merged) — sentinel can't see GitHub here; \
             confirm the PR actually exists and is in the claimed state \
             (`gh pr view`) before ✅"
                .to_string(),
        );
    }

    // --- Build/test claim: can't re-run → soft confirm note (only if claimed) ---
    if claims.build_test {
        warnings.push(
            "claims a passing build/test — sentinel can't re-run it here; \
             confirm the build/test was actually executed THIS session \
             (not assumed) before ✅"
                .to_string(),
        );
    }

    // --- Suspiciously-fast done: first in_progress hop + a concrete claim ---
    if first_in_progress && (claims.commit || claims.build_test) {
        warnings.push(
            "task is being completed on its FIRST in_progress hop while \
             claiming concrete work (commit/build/test) — double-check this \
             isn't a premature ✅"
                .to_string(),
        );
    }

    warnings
}

/// Process `TaskCompleted` event
///
/// Injects context reminding the teammate to verify before marking complete.
/// If the task subject contains `@linear:{ID}`, also injects Linear sync instructions.
/// Additionally scans the task subject+description for concrete-artifact claims
/// (commit/push, PR, build/test) and warns on any claim-vs-reality mismatch
/// (e.g. "committed" but the tree is still dirty). FAIL OPEN on any error.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    // SEN-1: drop malformed TaskCompleted events. If the upstream hook
    // dispatcher didn't populate task_id / task_subject / teammate_name with
    // real values, the event is malformed — emitting it just spams the
    // session with "Task #? completed: 'unknown task' (by unknown)" lines.
    let task_subject = match input.extra.get("task_subject").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() && s != "unknown task" => s,
        _ => return HookOutput::allow(),
    };

    let teammate_name = match input.extra.get("teammate_name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() && s != "unknown" => s,
        _ => return HookOutput::allow(),
    };

    let team_name = input
        .extra
        .get("team_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");

    let task_id = match input.extra.get("task_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() && s != "?" => s,
        _ => return HookOutput::allow(),
    };

    // Base verification reminder
    let mut context = format!(
        "[Task Completion Gate] Teammate '{teammate_name}' (team: {team_name}) is completing task #{task_id}: '{task_subject}'\n\
         \n\
         BEFORE marking this task complete, verify:\n\
         1. All acceptance criteria from the task description are met\n\
         2. Tests pass (run them, don't assume)\n\
         3. No TODO/FIXME/HACK markers left in changed code\n\
         4. Changes are committed (or staged for the lead to review)\n\
         5. Report what was done via SendMessage to the team lead"
    );

    // Check for incomplete checklist items
    if let Some(checklist) = input.extra.get("task_checklist").and_then(|v| v.as_array()) {
        if !checklist.is_empty() {
            let incomplete: Vec<&str> = checklist
                .iter()
                .filter(|item| {
                    !item
                        .get("completed")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                })
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                .collect();
            if !incomplete.is_empty() {
                let _ = write!(
                    context,
                    "\n\n⚠ [Checklist Warning] {} of {} checklist items are NOT completed:\n{}",
                    incomplete.len(),
                    checklist.len(),
                    incomplete
                        .iter()
                        .map(|t| format!("  - [ ] {t}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
        }
    }

    // Claim-vs-reality verification (catch "plausible-but-false done").
    // Scan subject + (optional) description for concrete-artifact claims, then
    // corroborate against the working tree. FAIL OPEN — wrapped so any panic or
    // git error can never block the completion gate from injecting context.
    let claim_warnings = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Optional richer description; harness may not populate it.
        let description = input
            .extra
            .get("task_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let scan_text = format!("{task_subject}\n{description}");
        let claims = detect_claims(&scan_text);

        // "Suspiciously fast done": this is the first time the task ever went
        // in_progress (claimed complete on the first hop). The harness may
        // expose an in_progress hop count; treat == 1 as first. Absent → false
        // (conservative: don't warn when we can't tell).
        let first_in_progress = input
            .extra
            .get("task_in_progress_count")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n == 1)
            .unwrap_or(false);

        let cwd = input.cwd.as_deref().unwrap_or(".");
        verify_claims(claims, ctx, cwd, first_in_progress)
    }))
    .unwrap_or_default();

    if !claim_warnings.is_empty() {
        let _ = write!(
            context,
            "\n\n⚠️ Verify before ✅: this task's text makes concrete claims \
             that don't (yet) hold:\n{}",
            claim_warnings
                .iter()
                .map(|w| format!("  - {w}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    // If task is bound to a Linear issue, append sync instructions
    if let Some(linear_id) = extract_linear_id(task_subject) {
        let _ = write!(
            context,
            "\n\n\
             [Linear Sync] Task is bound to Linear issue {linear_id}.\n\
             After verifying the task is complete:\n\
             1. Post a progress comment on {linear_id} via mcp__linear__create_comment\n\
             2. Check if ALL tasks with @linear:{linear_id} are now completed (use TaskList)\n\
             3. If all tasks done → transition the Linear issue to the next workflow state\n\
             4. If tasks remain → note progress in the comment (e.g., \"3/5 tasks complete\")"
        );
    }

    // Emit channel event for real-time push notification
    let summary = format!("Task #{task_id} completed: '{task_subject}' (by {teammate_name})");
    let mut meta = serde_json::Map::new();
    meta.insert(
        "task_id".to_string(),
        serde_json::Value::String(task_id.to_string()),
    );
    meta.insert(
        "task_subject".to_string(),
        serde_json::Value::String(task_subject.to_string()),
    );
    meta.insert(
        "teammate_name".to_string(),
        serde_json::Value::String(teammate_name.to_string()),
    );
    crate::channel_events::emit(
        ctx.fs,
        ctx.env,
        "task_completed",
        &summary,
        meta,
        input.session_id.as_deref(),
        input.cwd.as_deref(),
        Some("task_completed"),
    );

    // Keep the Active Tasks section of ~/.claude/CLAUDE.md in sync with live
    // task state. Completion removes the task from the rendered table, so
    // regenerate immediately. Fire-and-forget — a regen failure must never
    // prevent the verification-gate context from being injected below.
    let _ = std::panic::catch_unwind(super::session_init::regenerate_global_claude_md);

    HookOutput::inject_context(HookEvent::TaskCompleted, &context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::ports::GitStatusPort;

    /// Git stub with caller-chosen dirty flag + HEAD presence, and an option to
    /// make `has_uncommitted_changes` error (to exercise the fail-open path).
    struct FakeGit {
        dirty: bool,
        head: Option<String>,
        err: bool,
    }
    impl Default for FakeGit {
        fn default() -> Self {
            // Sensible default: a repo with HEAD, clean tree, no errors.
            FakeGit {
                dirty: false,
                head: Some("abc1234".to_string()),
                err: false,
            }
        }
    }
    impl GitStatusPort for FakeGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            if self.err {
                anyhow::bail!("git boom");
            }
            Ok(self.dirty)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn head_sha(&self, _: &str) -> Option<String> {
            self.head.clone()
        }
    }

    /// Build a HookContext using `git` and otherwise-stub ports.
    fn ctx_with_git(git: &dyn GitStatusPort) -> super::super::HookContext<'_> {
        let base = crate::hooks::test_support::stub_ctx();
        super::super::HookContext { git, ..base }
    }

    #[test]
    fn test_task_completed_injects_context() {
        let mut input = HookInput::default();
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("Implement auth"),
        );
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("auth-team"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("42"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("backend-dev"));
        assert!(ctx.contains("auth-team"));
        assert!(ctx.contains("Implement auth"));
        assert!(ctx.contains("#42"));
    }

    #[test]
    fn test_task_completed_drops_event_when_required_fields_missing() {
        // SEN-1: events without real task_id / task_subject / teammate_name
        // are malformed and must be dropped, not surfaced as
        // "Task #? completed: 'unknown task' (by unknown)" notifications.
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(
            output.hook_specific_output.is_none(),
            "malformed TaskCompleted must produce no output"
        );
    }

    #[test]
    fn test_task_completed_drops_event_when_fields_are_unknown_literals() {
        // SEN-1: also drop events where the dispatcher populated the literal
        // placeholders ("?", "unknown task", "unknown").
        let mut input = HookInput::default();
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("?"));
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("unknown task"),
        );
        input
            .extra
            .insert("teammate_name".to_string(), serde_json::json!("unknown"));
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_extract_linear_id_valid() {
        assert_eq!(
            extract_linear_id("[P1] Implement auth #feature @linear:FIR-123"),
            Some("FIR-123")
        );
    }

    #[test]
    fn test_extract_linear_id_end_of_string() {
        assert_eq!(extract_linear_id("Task @linear:SYN-42"), Some("SYN-42"));
    }

    #[test]
    fn test_extract_linear_id_missing() {
        assert_eq!(extract_linear_id("[P0] Fix bug #security"), None);
    }

    #[test]
    fn test_task_completed_with_incomplete_checklist() {
        let mut input = HookInput::default();
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("Build feature"),
        );
        input
            .extra
            .insert("teammate_name".to_string(), serde_json::json!("dev-1"));
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("team-a"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("5"));
        input.extra.insert(
            "task_checklist".to_string(),
            serde_json::json!([
                {"id": "1", "text": "Design API", "completed": true},
                {"id": "2", "text": "Write tests", "completed": false},
                {"id": "3", "text": "Update docs", "completed": false}
            ]),
        );

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("[Checklist Warning]"));
        assert!(ctx.contains("2 of 3"));
        assert!(ctx.contains("Write tests"));
        assert!(ctx.contains("Update docs"));
    }

    #[test]
    fn test_task_completed_with_linear_tag() {
        let mut input = HookInput::default();
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("[P1] Implement auth @linear:FIR-123"),
        );
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("fir-team"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("7"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("[Linear Sync]"));
        assert!(ctx.contains("FIR-123"));
        assert!(ctx.contains("mcp__linear__create_comment"));
    }

    // ---- Claim-verification (claim-vs-reality) tests ----

    /// Build a minimal valid TaskCompleted input with the given subject.
    fn claim_input(subject: &str) -> HookInput {
        let mut input = HookInput::default();
        input
            .extra
            .insert("task_subject".to_string(), serde_json::json!(subject));
        input
            .extra
            .insert("teammate_name".to_string(), serde_json::json!("dev"));
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("team"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("99"));
        input
    }

    fn injected(out: &HookOutput) -> String {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
            .unwrap_or_default()
    }

    #[test]
    fn detect_claims_finds_signals() {
        assert!(detect_claims("Committed the fix").commit);
        assert!(detect_claims("Pushed to feature branch").commit);
        assert!(detect_claims("landed at deadbe1f").commit); // sha-ish
        assert!(detect_claims("Opened PR #42").pr);
        assert!(detect_claims("merged pull request").pr);
        assert!(detect_claims("tests pass, build clean").build_test);
        assert!(detect_claims("all 12 passed").build_test);
        // No concrete-artifact claim.
        let none = detect_claims("Investigate the slow query");
        assert!(!none.any());
    }

    #[test]
    fn contains_sha_requires_digit_and_length() {
        assert!(contains_sha("abc1234"));
        assert!(contains_sha("deadbe1f cafe"));
        assert!(!contains_sha("abc")); // too short
        assert!(!contains_sha("deadbeef")); // hex but no digit
        assert!(!contains_sha("just words here"));
    }

    #[test]
    fn claim_commit_clean_tree_no_warn() {
        // Claims a commit, tree is CLEAN → no claim-mismatch warning.
        let input = claim_input("Committed the auth fix");
        let git = FakeGit {
            dirty: false,
            ..FakeGit::default()
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("⚠️ Verify before ✅"),
            "clean tree must not trigger the claim-mismatch warning: {text}"
        );
    }

    #[test]
    fn claim_commit_dirty_tree_warns() {
        // Claims a commit, tree is DIRTY → warn (the key false-done check).
        let input = claim_input("Committed and pushed the auth fix");
        let git = FakeGit {
            dirty: true,
            head: Some("abc1234".to_string()),
            err: false,
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("UNCOMMITTED"), "{text}");
    }

    #[test]
    fn no_claim_no_warn() {
        // No concrete-artifact claim → no claim-mismatch warning, even dirty.
        let input = claim_input("Investigate the flaky route handler");
        let git = FakeGit {
            dirty: true,
            ..FakeGit::default()
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("⚠️ Verify before ✅"),
            "no claim must not warn: {text}"
        );
    }

    #[test]
    fn fail_open_on_git_error() {
        // Claims a commit, but git errors → fail open (no hard warning).
        let input = claim_input("Committed the fix");
        let git = FakeGit {
            dirty: false,
            head: Some("abc1234".to_string()),
            err: true,
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        // Must not crash, and must not emit the commit-mismatch warning.
        assert!(
            !text.contains("UNCOMMITTED"),
            "git error must fail open (no commit-mismatch warning): {text}"
        );
    }

    #[test]
    fn commit_claim_no_head_no_warn() {
        // Claims a commit, tree dirty, but HEAD absent (unborn / not a repo) →
        // can't verify → stay silent.
        let input = claim_input("Committed the fix");
        let git = FakeGit {
            dirty: true,
            head: None,
            err: false,
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(!text.contains("UNCOMMITTED"), "{text}");
    }

    #[test]
    fn pr_claim_emits_soft_confirm() {
        let input = claim_input("Opened PR #123 for the refactor");
        let git = FakeGit::default();
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("gh pr view"), "{text}");
    }

    #[test]
    fn build_test_claim_emits_soft_confirm() {
        let input = claim_input("tests pass and build clean");
        let git = FakeGit::default();
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("build/test"), "{text}");
    }

    #[test]
    fn first_in_progress_hop_with_claim_warns_premature() {
        let mut input = claim_input("Committed the fix");
        input.extra.insert(
            "task_in_progress_count".to_string(),
            serde_json::json!(1),
        );
        let git = FakeGit::default(); // clean tree, has HEAD
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("FIRST in_progress hop"), "{text}");
    }
}
