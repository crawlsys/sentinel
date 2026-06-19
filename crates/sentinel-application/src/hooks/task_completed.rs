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
// Four independent claim signals, each a presence flag scanned from task text.
// These are orthogonal observations (not states of one machine), so a flat
// bool-per-signal bag is the correct shape — a "two-variant enum" refactor
// would obscure, not clarify.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClaimSignals {
    /// Text claims a commit/push happened ("committed", "pushed", a sha, ✅).
    pub(crate) commit: bool,
    /// Text references a PR ("PR #N", "pull request", "opened/merged PR").
    pub(crate) pr: bool,
    /// Text claims a build/test outcome ("build clean", "tests pass", "N passed").
    pub(crate) build_test: bool,
    /// Text emitted an explicit completion-promise terminal string
    /// (`ALL_TESTS_PASSING`, `IMPLEMENTATION_COMPLETE`, …). Praetorian-style
    /// "no fuzzy interpretation" done-signal: the agent must say the exact word.
    /// On its own a promise proves nothing — it is the STRONGEST done-claim, so
    /// an emitted promise with zero corroborating ground truth is a hard
    /// mismatch (see `verify_claims`).
    pub(crate) completion_promise: bool,
}

impl ClaimSignals {
    pub(crate) fn any(self) -> bool {
        self.commit || self.pr || self.build_test || self.completion_promise
    }
}

/// The exact terminal strings sentinel treats as an explicit completion
/// promise. Matched case-insensitively as substrings of the task text. Kept
/// deliberately small and unambiguous — these are protocol tokens an agent
/// emits to assert "done", not natural-language phrases.
pub(crate) const COMPLETION_PROMISE_MARKERS: &[&str] = &[
    "all_tests_passing",
    "implementation_complete",
    "all_checks_passed",
    "verification_complete",
];

/// Return the first completion-promise marker found in the (already-lowercased)
/// text, if any. Used both to set the `completion_promise` signal and to name
/// the specific marker in the mismatch warning.
fn extract_completion_promise(lower: &str) -> Option<&'static str> {
    COMPLETION_PROMISE_MARKERS
        .iter()
        .find(|m| lower.contains(*m))
        .copied()
}

/// Scan a task's subject + description for concrete-artifact claim signals.
///
/// Lowercased substring matching — deliberately cheap and heuristic. The goal
/// is to catch the obvious "plausible-but-false done" phrasings, not to parse
/// natural language. False negatives (missed claims) are fine; we only act on
/// a *mismatch*, so a missed claim just means no warning.
pub(crate) fn detect_claims(text: &str) -> ClaimSignals {
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

    let completion_promise = extract_completion_promise(&lower).is_some();

    ClaimSignals {
        commit,
        pr,
        build_test,
        completion_promise,
    }
}

/// Heuristic: does the (already-lowercased) text contain a git-sha-looking
/// token — a standalone run of 7–40 hex chars with at least one digit? Used as
/// a weak commit signal. Requiring a digit avoids tripping on plain hex-only
/// words.
fn contains_sha(lower: &str) -> bool {
    extract_sha(lower).is_some()
}

/// Extract the first git-sha-looking token from text — a standalone run of
/// 7–40 hex chars containing at least one digit. Returns the matched token
/// (lowercased by the caller's input if already lowercased). Used to drive the
/// REAL `git merge-base --is-ancestor` reachability check.
fn extract_sha(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|tok| {
            let n = tok.len();
            (7..=40).contains(&n)
                && tok.chars().all(|c| c.is_ascii_hexdigit())
                && tok.chars().any(|c| c.is_ascii_digit())
        })
        .map(str::to_string)
}

/// A PR reference extracted from task text: its number, and optionally the
/// `owner/repo` slug when the reference came from a full GitHub URL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrRef {
    number: u32,
    /// `Some(("owner", "repo"))` when parsed from a github.com/owner/repo URL.
    repo: Option<(String, String)>,
}

/// Extract a PR reference from task text. Recognises (in priority order):
///   1. A full GitHub PR URL: `https://github.com/OWNER/REPO/pull/N` → repo + N.
///   2. A bare `PR #N` / `pull request #N` / `pull/N` form → just N.
///
/// Returns the first match found. FAIL-OPEN friendly: returns `None` when no
/// number can be parsed (caller then skips the gh check).
fn extract_pr(text: &str) -> Option<PrRef> {
    // 1) Full URL form — github.com/owner/repo/pull/123
    if let Some(idx) = text.find("github.com/") {
        let rest = &text[idx + "github.com/".len()..];
        let mut segs = rest.split('/');
        if let (Some(owner), Some(repo), Some(kw), Some(num)) =
            (segs.next(), segs.next(), segs.next(), segs.next())
        {
            if (kw == "pull" || kw == "pulls") && !owner.is_empty() && !repo.is_empty() {
                if let Some(n) = parse_leading_u32(num) {
                    return Some(PrRef {
                        number: n,
                        repo: Some((owner.to_string(), repo.to_string())),
                    });
                }
            }
        }
    }

    // 2) Bare forms — find a `#N` or `pull/N` / `pull request N`.
    let lower = text.to_lowercase();
    // `#N`
    if let Some(hash) = lower.find('#') {
        if let Some(n) = parse_leading_u32(&text[hash + 1..]) {
            return Some(PrRef {
                number: n,
                repo: None,
            });
        }
    }
    // `pull/N`
    if let Some(p) = lower.find("pull/") {
        if let Some(n) = parse_leading_u32(&text[p + "pull/".len()..]) {
            return Some(PrRef {
                number: n,
                repo: None,
            });
        }
    }
    None
}

/// Parse a leading run of ASCII digits from `s` into a `u32`. Returns `None`
/// when `s` does not begin with a digit or the number overflows.
fn parse_leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Does the task text claim the PR/work was *merged* (vs merely opened)?
fn claims_merged(lower: &str) -> bool {
    lower.contains("merged")
        || lower.contains("merge to")
        || lower.contains("merged to")
        || lower.contains("landed")
}

/// Run a short git/gh subprocess via the process port, FAIL-OPEN.
///
/// Returns `Some(ProcessOutput)` only when the command actually executed (even
/// if it exited non-zero — the caller inspects `.success`). Returns `None` when
/// the binary is missing / not a repo / any spawn error, so the caller can fall
/// back to the soft heuristic and never blocks the gate.
///
/// There is no per-call timeout primitive on `ProcessPort::run`; the commands
/// used here are deliberately fast & bounded (`git merge-base --is-ancestor` is
/// a local ancestry walk, `gh pr view --json` is a single bounded API call with
/// gh's own internal HTTP timeout). Any hang/error surfaces as `Err` → `None`.
fn run_check(
    ctx: &super::HookContext<'_>,
    cmd: &str,
    args: &[&str],
    cwd: &str,
) -> Option<sentinel_domain::ports::ProcessOutput> {
    ctx.process.run(cmd, args, Some(cwd)).ok()
}

/// Run the claim-vs-reality checks and return any mismatch warning lines.
///
/// Performs REAL verification via `ctx.process` where possible:
///   * COMMIT — a concrete sha is checked for reachability on HEAD with
///     `git merge-base --is-ancestor <sha> HEAD`; otherwise the dirty-tree
///     verification is surfaced when no concrete sha is named.
///   * PR — the claimed PR is checked with `gh pr view <N> --json state,merged`
///     (with `--repo owner/repo` when a URL supplied one); a claimed-merged PR
///     that gh reports as open/closed is a hard mismatch.
///   * BUILD/TEST — can't be re-run here → soft confirm note (unchanged).
///
/// Verification-unavailable states are reported as explicit warnings instead
/// of being silently downgraded.
///
/// `first_in_progress` is true when this is the first time the task entered
/// `in_progress` (suspiciously fast done — claimed complete on the first hop).
pub(crate) fn verify_claims(
    claims: ClaimSignals,
    text: &str,
    ctx: &super::HookContext<'_>,
    cwd: &str,
    first_in_progress: bool,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();

    if !claims.any() {
        return warnings;
    }

    let lower = text.to_lowercase();

    // Track whether any GROUND-TRUTH corroboration actually fired this pass.
    // A completion-promise (the strongest done-claim) with zero corroboration
    // is a hard mismatch — see the completion-promise check below.
    let mut corroborated = false;

    // --- Commit/push claim: the KEY high-value check ---
    if claims.commit {
        // If a SPECIFIC sha is named, do the REAL reachability check: is it an
        // ancestor of HEAD? `git merge-base --is-ancestor <sha> HEAD` exits 0
        // when reachable, 1 when not.
        let real_sha_checked = if let Some(sha) = extract_sha(&lower) {
            match run_check(
                ctx,
                "git",
                &["merge-base", "--is-ancestor", &sha, "HEAD"],
                cwd,
            ) {
                Some(out) if out.success => {
                    corroborated = true; // sha reachable from HEAD → real ground truth
                    true
                }
                Some(out) => {
                    // Exit non-zero. Distinguish "not an ancestor" (the bad
                    // case) from "bad object / unknown rev" (can't verify).
                    // git prints to stderr on a bad rev; --is-ancestor on a
                    // valid-but-unreachable rev exits 1 with empty stderr.
                    let stderr = out.stderr.to_lowercase();
                    let unresolved = stderr.contains("not a valid")
                        || stderr.contains("bad revision")
                        || stderr.contains("unknown revision")
                        || stderr.contains("malformed")
                        || stderr.contains("ambiguous argument");
                    if unresolved {
                        warnings.push(format!(
                            "claimed commit {sha}, but git could not resolve that sha in this \
                             repository — confirm the cited commit exists on the checked-out \
                             history before ✅"
                        ));
                        true
                    } else {
                        warnings.push(format!(
                            "claimed commit {sha} is NOT on HEAD's history \
                             (`git merge-base --is-ancestor {sha} HEAD` failed) — \
                             the commit may live on another branch, or was never \
                             made; confirm before ✅"
                        ));
                        true
                    }
                }
                None => {
                    warnings.push(format!(
                        "claimed commit {sha}, but sentinel could not run git reachability \
                         verification here — confirm the commit is on HEAD's history before ✅"
                    ));
                    true
                }
            }
        } else {
            false
        };

        // No specific sha verified → require a clean, valid git working tree.
        // "Committed" while the tree is dirty or git state is unavailable is a
        // visible verification warning.
        if !real_sha_checked {
            match ctx.git.has_uncommitted_changes(cwd) {
                Ok(dirty) => {
                    let has_head = ctx.git.head_sha(cwd).is_some();
                    if dirty && has_head {
                        warnings.push(
                            "claims a commit/push, but the working tree still has \
                             UNCOMMITTED changes — confirm the work was actually committed \
                             (run `git status`), not just edited"
                                .to_string(),
                        );
                    } else if !has_head {
                        warnings.push(
                            "claims a commit/push, but sentinel could not read HEAD for this \
                             repository — confirm the work is committed on a real branch before ✅"
                                .to_string(),
                        );
                    }
                }
                Err(err) => warnings.push(format!(
                    "claims a commit/push, but sentinel could not inspect git status ({err}) — \
                     confirm the working tree and commit history before ✅"
                )),
            }
        }
    }

    // --- PR claim: REAL gh check when a PR number is parseable ---
    if claims.pr {
        if let Some(pr) = extract_pr(text) {
            let n = pr.number.to_string();
            let mut args: Vec<&str> = vec!["pr", "view", &n, "--json", "state,merged,number"];
            let repo_slug; // keep alive for the &str borrow
            if let Some((owner, repo)) = &pr.repo {
                repo_slug = format!("{owner}/{repo}");
                args.push("--repo");
                args.push(&repo_slug);
            }

            match run_check(ctx, "gh", &args, cwd) {
                Some(out) if out.success => {
                    // gh returned JSON describing the PR. Parse merged/state.
                    let merged = serde_json::from_str::<serde_json::Value>(&out.stdout)
                        .ok()
                        .and_then(|v| v.get("merged").and_then(serde_json::Value::as_bool));
                    let state = serde_json::from_str::<serde_json::Value>(&out.stdout)
                        .ok()
                        .and_then(|v| {
                            v.get("state")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "UNKNOWN".to_string());

                    if claims_merged(&lower) {
                        match merged {
                            Some(true) => corroborated = true, // PR really merged → ground truth
                            _ => warnings.push(format!(
                                "PR #{n} is claimed MERGED but gh shows it is \
                                 {state} (merged=false) — do NOT ✅ until the PR \
                                 is actually merged"
                            )),
                        }
                    }
                    // Non-merge PR claims (just "opened PR #N") are corroborated
                    // by gh finding the PR at all → no warning.
                    else {
                        corroborated = true; // gh found the PR → the reference is real
                    }
                }
                Some(out) => {
                    // gh ran but failed (PR not found, no repo, auth). Soft note.
                    let err = first_line(&out.stderr);
                    warnings.push(format!(
                        "could not verify PR #{n} (gh: {err}) — confirm the PR \
                         exists and is in the claimed state (`gh pr view {n}`) before ✅"
                    ));
                }
                None => {
                    // gh missing / spawn error → verification-unavailable note.
                    warnings.push(format!(
                        "references PR #{n} but sentinel could not run gh here — \
                         confirm the PR exists and is in the claimed state \
                         (`gh pr view {n}`) before ✅"
                    ));
                }
            }
        } else {
            // PR claimed but no number parseable → soft confirm note.
            warnings.push(
                "references a PR (opened/merged) — sentinel couldn't parse a PR \
                 number; confirm the PR actually exists and is in the claimed \
                 state (`gh pr view`) before ✅"
                    .to_string(),
            );
        }
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

    // --- Completion promise: an explicit terminal string MUST be backed by
    // ground truth. The promise is the strongest done-claim ("no fuzzy
    // interpretation") — so emitting it while NOTHING verifiable corroborated
    // the work (no reachable commit, no found/merged PR) is a hard mismatch,
    // not a soft note. A passing-build claim alone does NOT count: sentinel
    // can't re-run it, so it can't corroborate a promise. ---
    if claims.completion_promise && !corroborated {
        let marker = extract_completion_promise(&lower).unwrap_or("a completion promise");
        warnings.push(format!(
            "emitted completion promise `{}` but NO corroborating ground truth \
             verified (no reachable commit, no found/merged PR) — an explicit \
             done-signal with nothing to back it is exactly the false-done \
             pattern; do NOT ✅ until the work is verifiable",
            marker.to_uppercase()
        ));
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

/// First non-empty line of a (possibly multi-line) error string, trimmed and
/// length-capped — keeps gh's stderr from flooding the injected context.
fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error");
    if line.len() > 200 {
        format!("{}…", &line[..200])
    } else {
        line.to_string()
    }
}

/// Process `TaskCompleted` event
///
/// Injects context reminding the teammate to verify before marking complete.
/// If the task subject contains `@linear:{ID}`, also injects Linear sync instructions.
/// Additionally scans the task subject+description for concrete-artifact claims
/// (commit/push, PR, build/test) and warns on any claim-vs-reality mismatch
/// (e.g. "committed" but the tree is still dirty).
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
    // corroborate against the working tree. Wrapped so a panic cannot prevent
    // the completion context from being injected; panic becomes a visible
    // verification-unavailable warning.
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
            == Some(1);

        let cwd = input.cwd.as_deref().unwrap_or(".");
        verify_claims(claims, &scan_text, ctx, cwd, first_in_progress)
    }))
    .unwrap_or_else(|_| {
        vec![
            "sentinel claim verification failed internally — confirm the task's commit/PR/build \
             claims manually before ✅"
                .to_string(),
        ]
    });

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
    /// make `has_uncommitted_changes` error.
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
        fn has_uncommitted_changes(
            &self,
            _: &str,
        ) -> Result<bool, sentinel_domain::port_errors::GitError> {
            if self.err {
                return Err(sentinel_domain::port_errors::GitError::backend("git boom"));
            }
            Ok(self.dirty)
        }
        fn changed_files(
            &self,
            _: &str,
        ) -> Result<Vec<String>, sentinel_domain::port_errors::GitError> {
            Ok(vec![])
        }
        fn current_branch(
            &self,
            _: &str,
        ) -> Result<String, sentinel_domain::port_errors::GitError> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(
            &self,
            _: &str,
        ) -> Result<bool, sentinel_domain::port_errors::GitError> {
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
        fn rev_list_count_range(&self, _: &str, _: &str) -> Option<u32> {
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

    use sentinel_domain::ports::{ProcessOutput, ProcessPort};

    /// Programmable process port for the real-check tests.
    ///
    /// Each closure-free field controls one command family:
    ///   * `git_missing` / `gh_missing` — `run` returns `Err` (binary absent /
    ///     spawn failure), which should surface as a verification warning.
    ///   * `merge_base_ancestor` — `Some(true)`=exit0 (reachable),
    ///     `Some(false)`=exit1 (not an ancestor, empty stderr),
    ///     `None`=unresolved rev (exit1 with a "bad revision" stderr).
    ///   * `gh_stdout` / `gh_success` — what `gh pr view` returns.
    struct FakeProcess {
        git_missing: bool,
        gh_missing: bool,
        merge_base_ancestor: Option<bool>,
        gh_success: bool,
        gh_stdout: String,
        gh_stderr: String,
    }
    impl Default for FakeProcess {
        fn default() -> Self {
            FakeProcess {
                git_missing: false,
                gh_missing: false,
                merge_base_ancestor: Some(true),
                gh_success: true,
                gh_stdout: String::new(),
                gh_stderr: String::new(),
            }
        }
    }
    impl ProcessPort for FakeProcess {
        fn run(
            &self,
            command: &str,
            args: &[&str],
            _cwd: Option<&str>,
        ) -> Result<ProcessOutput, sentinel_domain::port_errors::ProcessError> {
            match command {
                "git" if self.git_missing => Err(
                    sentinel_domain::port_errors::ProcessError::backend("git: command not found"),
                ),
                "gh" if self.gh_missing => Err(
                    sentinel_domain::port_errors::ProcessError::backend("gh: command not found"),
                ),
                "git" if args.first() == Some(&"merge-base") => match self.merge_base_ancestor {
                    Some(true) => Ok(ProcessOutput {
                        success: true,
                        stdout: String::new(),
                        stderr: String::new(),
                    }),
                    Some(false) => Ok(ProcessOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: String::new(), // valid rev, just not an ancestor
                    }),
                    None => Ok(ProcessOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "fatal: Not a valid commit name deadbe1f".to_string(),
                    }),
                },
                "gh" => Ok(ProcessOutput {
                    success: self.gh_success,
                    stdout: self.gh_stdout.clone(),
                    stderr: self.gh_stderr.clone(),
                }),
                // Anything else → benign success.
                _ => Ok(ProcessOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }
        fn spawn_detached(
            &self,
            _: &str,
            _: &[&str],
        ) -> Result<(), sentinel_domain::port_errors::ProcessError> {
            Ok(())
        }
    }

    /// Build a HookContext using `git` and otherwise-stub ports.
    fn ctx_with_git(git: &dyn GitStatusPort) -> super::super::HookContext<'_> {
        let base = crate::hooks::test_support::stub_ctx();
        super::super::HookContext { git, ..base }
    }

    /// Build a HookContext with both a custom git and a custom process port.
    fn ctx_with<'a>(
        git: &'a dyn GitStatusPort,
        process: &'a dyn ProcessPort,
    ) -> super::super::HookContext<'a> {
        let base = crate::hooks::test_support::stub_ctx();
        super::super::HookContext {
            git,
            process,
            ..base
        }
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
    fn detect_claims_finds_completion_promise() {
        assert!(detect_claims("ALL_TESTS_PASSING").completion_promise);
        assert!(detect_claims("done — IMPLEMENTATION_COMPLETE").completion_promise);
        assert!(detect_claims("all_checks_passed").completion_promise); // case-insensitive
        assert!(detect_claims("VERIFICATION_COMPLETE ✅").completion_promise);
        // A natural-language "all tests pass" is NOT the protocol token.
        assert!(!detect_claims("all the tests are passing now").completion_promise);
        // any() must now also fire on a lone promise.
        assert!(detect_claims("IMPLEMENTATION_COMPLETE").any());
    }

    #[test]
    fn completion_promise_without_corroboration_is_hard_mismatch() {
        // Promise emitted, clean tree, NO sha / NO PR → nothing corroborates it.
        let input = claim_input("IMPLEMENTATION_COMPLETE — shipped the feature");
        let git = FakeGit {
            dirty: false,
            ..FakeGit::default()
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("emitted completion promise"), "{text}");
        assert!(text.contains("IMPLEMENTATION_COMPLETE"), "{text}");
        // (The Stop-sweep hard-mismatch keying on this phrase is covered by
        // `claim_reality_check::tests::is_hard_mismatch_discriminates`.)
    }

    #[test]
    fn completion_promise_with_reachable_sha_is_clean() {
        // Promise + a sha that IS reachable from HEAD → corroborated, no promise warning.
        let input = claim_input("ALL_TESTS_PASSING at abc1234");
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(true), // sha reachable
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("emitted completion promise"),
            "reachable sha must corroborate the promise: {text}"
        );
    }

    #[test]
    fn completion_promise_with_merged_pr_is_clean() {
        // Promise + a PR gh confirms MERGED → corroborated, no promise warning.
        let input = claim_input("VERIFICATION_COMPLETE — merged PR #42");
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_success: true,
            gh_stdout: r#"{"number":42,"state":"MERGED","merged":true}"#.to_string(),
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("emitted completion promise"),
            "merged PR must corroborate the promise: {text}"
        );
    }

    #[test]
    fn completion_promise_with_only_buildtest_claim_still_mismatches() {
        // A passing-build CLAIM is a soft note — it cannot corroborate a promise
        // (sentinel can't re-run it), so the promise is still a hard mismatch.
        let input = claim_input("ALL_TESTS_PASSING — tests pass, build clean");
        let git = FakeGit::default();
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            text.contains("emitted completion promise"),
            "build/test claim must not corroborate a promise: {text}"
        );
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
    fn git_status_error_warns() {
        // Claims a commit, but git status errors → verification warning.
        let input = claim_input("Committed the fix");
        let git = FakeGit {
            dirty: false,
            head: Some("abc1234".to_string()),
            err: true,
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("could not inspect git status"), "{text}");
    }

    #[test]
    fn commit_claim_no_head_warns() {
        // Claims a commit, tree dirty, but HEAD absent (unborn / not a repo) →
        // verification warning.
        let input = claim_input("Committed the fix");
        let git = FakeGit {
            dirty: true,
            head: None,
            err: false,
        };
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("could not read HEAD"), "{text}");
    }

    #[test]
    fn pr_claim_gh_finds_open_pr_no_merge_claim_no_warn() {
        // "Opened PR #123" — not a merge claim. gh finds the PR → corroborated,
        // no warning.
        let input = claim_input("Opened PR #123 for the refactor");
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_success: true,
            gh_stdout: r#"{"number":123,"state":"OPEN","merged":false}"#.to_string(),
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("⚠️ Verify before ✅"),
            "open PR with no merge claim must not warn: {text}"
        );
    }

    #[test]
    fn pr_claim_gh_not_found_emits_soft_confirm() {
        // gh ran but the PR doesn't exist → soft "could not verify" note.
        let input = claim_input("Opened PR #123 for the refactor");
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_success: false,
            gh_stderr: "GraphQL: Could not resolve to a PullRequest with the number of 123."
                .to_string(),
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("could not verify PR #123"), "{text}");
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
        input
            .extra
            .insert("task_in_progress_count".to_string(), serde_json::json!(1));
        let git = FakeGit::default(); // clean tree, has HEAD
        let ctx = ctx_with_git(&git);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("FIRST in_progress hop"), "{text}");
    }

    // ---- REAL git/gh check tests ----

    #[test]
    fn extract_sha_finds_first_token() {
        assert_eq!(
            extract_sha("landed at deadbe1f now").as_deref(),
            Some("deadbe1f")
        );
        assert_eq!(extract_sha("sha abc1234 done").as_deref(), Some("abc1234"));
        assert_eq!(extract_sha("no sha here"), None);
        assert_eq!(extract_sha("deadbeef"), None); // hex but no digit
    }

    #[test]
    fn extract_pr_parses_bare_and_url() {
        assert_eq!(
            extract_pr("Opened PR #42 for the fix"),
            Some(PrRef {
                number: 42,
                repo: None
            })
        );
        assert_eq!(
            extract_pr("see https://github.com/legatus-ai/sentinel/pull/146 merged"),
            Some(PrRef {
                number: 146,
                repo: Some(("legatus-ai".to_string(), "sentinel".to_string()))
            })
        );
        assert_eq!(extract_pr("no pr referenced"), None);
    }

    #[test]
    fn real_sha_on_head_no_warn() {
        // Task names a sha that IS reachable on HEAD (merge-base exit 0) → no warn.
        let input = claim_input("Committed at deadbe1f and verified");
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(true),
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("⚠️ Verify before ✅"),
            "sha on HEAD must not warn: {text}"
        );
    }

    #[test]
    fn bogus_sha_not_ancestor_warns() {
        // Task names a sha that resolves but is NOT an ancestor of HEAD → warn.
        let input = claim_input("Committed at deadbe1f on the branch");
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(false), // exit 1, empty stderr = valid but unreachable
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("NOT on HEAD"), "{text}");
        assert!(text.contains("deadbe1f"), "{text}");
    }

    #[test]
    fn unresolved_sha_warns() {
        // Sha doesn't resolve (bad object) → verification warning.
        let input = claim_input("Committed at deadbe1f");
        let git = FakeGit::default(); // clean tree
        let proc = FakeProcess {
            merge_base_ancestor: None, // exit 1 with "Not a valid commit name" stderr
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("could not resolve"), "{text}");
        assert!(!text.contains("NOT on HEAD"), "{text}");
    }

    #[test]
    fn pr_claimed_merged_but_gh_shows_open_warns() {
        // The headline real-check: claim says merged, gh shows OPEN → hard warn.
        let input = claim_input("Merged PR #42 to main");
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_success: true,
            gh_stdout: r#"{"number":42,"state":"OPEN","merged":false}"#.to_string(),
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("PR #42 is claimed MERGED"), "{text}");
        assert!(text.contains("OPEN"), "{text}");
    }

    #[test]
    fn pr_claimed_merged_and_gh_shows_merged_no_warn() {
        let input = claim_input("Merged PR #42 to main");
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_success: true,
            gh_stdout: r#"{"number":42,"state":"MERGED","merged":true}"#.to_string(),
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("⚠️ Verify before ✅"),
            "a genuinely-merged PR must not warn: {text}"
        );
    }

    #[test]
    fn gh_missing_warns_with_soft_note() {
        // gh binary absent → run errors → soft note (NOT silence,
        // since a PR was explicitly claimed), and must never panic/block.
        let input = claim_input("Merged PR #42 to main");
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_missing: true,
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        // Verification-unavailable: a soft confirm note, but NOT the hard
        // "claimed MERGED" warn.
        assert!(
            !text.contains("PR #42 is claimed MERGED"),
            "gh missing must not produce the hard mismatch warning: {text}"
        );
        assert!(
            text.contains("could not run gh"),
            "gh missing should surface a soft confirm note: {text}"
        );
    }

    #[test]
    fn git_missing_commit_sha_warns() {
        // Named sha but git binary absent → verification warning.
        let input = claim_input("Committed at deadbe1f");
        let git = FakeGit::default(); // clean tree
        let proc = FakeProcess {
            git_missing: true,
            ..FakeProcess::default()
        };
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(
            !text.contains("NOT on HEAD"),
            "git missing must not produce the ancestor-mismatch warning: {text}"
        );
        assert!(text.contains("⚠️ Verify before ✅"), "{text}");
        assert!(text.contains("could not run git reachability"), "{text}");
    }

    #[test]
    fn no_claim_no_real_checks_no_warn() {
        // No concrete claim → verify_claims returns early, no git/gh spawned.
        let input = claim_input("Investigate the slow query path");
        let git = FakeGit::default();
        let proc = FakeProcess::default();
        let ctx = ctx_with(&git, &proc);
        let out = process(&input, &ctx);
        let text = injected(&out);
        assert!(!text.contains("⚠️ Verify before ✅"), "{text}");
    }
}
