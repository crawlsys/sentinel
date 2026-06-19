//! Self-annealing meta-agent — Stop hook (Praetorian-inspired)
//!
//! "Hooks that evolve from the failures they catch." When a phase's quality
//! gate fails repeatedly, the system should learn to prevent that specific
//! bypass forever. Sentinel already records the forensic signal competitors
//! lack: `SessionState.failed_submissions` maps each `skill:phase` to a
//! [`SubmissionAttempts`] count. This hook reads that record and, on the Nth
//! consecutive failure of a phase, surfaces a self-annealing remediation.
//!
//! ## Two halves
//!
//! 1. **Detect (always on).** Emit a `[Self-Annealing]` envelope naming the
//!    repeatedly-failing phase(s) and nudging a `TaskCreate` to harden the
//!    skill — mirrors [`good_citizen_observer`](super::good_citizen_observer),
//!    pure context injection, zero writes.
//!
//! 2. **Auto-patch + PR (operator-armed).** ONLY when
//!    `SENTINEL_ALLOW_SELF_ANNEAL=1` is set in the operator's environment
//!    (fixed at Claude Code launch — an injected tool result cannot set it),
//!    the hook may append a conservative anti-pattern stanza to the *diagnosed
//!    skill file* and open a `[Self-Annealing]` PR for human review.
//!
//! ## Safety (this hook edits enforcement — it must never weaken it)
//!
//! The auto-patch half is gated three ways, all required:
//! - **Armed:** default OFF; `SENTINEL_ALLOW_SELF_ANNEAL=1` must be set.
//! - **Protected-path-safe:** [`self_anneal_patch_allowed`] is default-deny and
//!   permits ONLY a real skill `.md` file — never a phase file, never anything
//!   under `~/.claude/sentinel/`, never `settings.json`, never `crates/`
//!   source, never the engine binary. Even when armed, a protected path is
//!   refused. This composes with — does not bypass — the Attack #201 guard.
//! - **PR-only:** changes go to a fresh `self-annealing/*` branch and a PR,
//!   never a direct commit to the working branch or main; a human reviews.
//!
//! Everything fails open: any IO/parse error leaves the session unblocked.

use sentinel_domain::events::{HookEnvelope, HookEvent, HookInput, HookOutput, HookTier};
use sentinel_domain::state::{SessionState, SubmissionAttempts};

/// Consecutive gate failures for a phase before self-annealing engages.
pub const SELF_ANNEAL_FAILURE_THRESHOLD: u32 = 3;

/// Environment flag that arms the auto-patch + PR half. Default off.
pub const SELF_ANNEAL_ARM_ENV: &str = "SENTINEL_ALLOW_SELF_ANNEAL";

/// Is this phase a self-annealing candidate — has its gate failed enough
/// consecutive times to justify hardening the skill?
#[must_use]
pub fn is_annealing_candidate(attempts: &SubmissionAttempts, threshold: u32) -> bool {
    attempts.count >= threshold
}

/// **Load-bearing safety predicate.** May the auto-patch half write to
/// `target` (a forward-slash-normalized path)? Default-deny: returns true ONLY
/// when armed AND the target is a genuine skill definition `.md` and is NOT any
/// protected/enforcement path. Mirrors the protected-path rules the Attack #201
/// guard uses, so a self-annealing patch can never reach a path that could
/// neuter a running engine — even when armed.
#[must_use]
pub fn self_anneal_patch_allowed(target_norm: &str, armed: bool) -> bool {
    if !armed {
        return false;
    }
    let lower = target_norm.to_lowercase();

    // Must be a skill markdown file under a skills/ tree.
    let is_skill_md = lower.contains("/skills/") && lower.ends_with(".md");
    if !is_skill_md {
        return false;
    }

    // Default-deny everything that is protected / could weaken enforcement,
    // regardless of armed state:
    let forbidden = lower.contains("/phases/")              // phase files (Attack #201)
        || lower.contains("/.claude/sentinel/")            // live config/state
        || lower.ends_with("settings.json")                 // hook registrations
        || lower.contains("/crates/")                       // sentinel source
        || lower.contains("sentinel-engine")                // the engine binary
        || lower.contains("/.cargo/bin")                    // installed binaries
        || lower.contains("/config/workflows.toml")         // workflow policy
        || lower.contains("/config/hooks.toml"); // hook policy
    if forbidden {
        return false;
    }

    true
}

/// Deterministic, conservative anti-pattern stanza appended to a skill file
/// when annealing. It documents a hard constraint (the repeatedly-skipped
/// phase) without inventing prose — the genuine hardening is "this gate was
/// bypassed N times; it is not optional." A human refines it in the PR.
#[must_use]
pub fn anti_pattern_stanza(phase_key: &str, count: u32) -> String {
    format!(
        "\n\n<!-- [Self-Annealing] auto-inserted -->\n\
         ## Hard constraint (self-annealed)\n\n\
         The `{phase_key}` gate failed **{count}** times in one session — a sign \
         an agent repeatedly rationalized skipping or short-circuiting it. This \
         step is **not optional**. If you think \"this is simple enough to skip\", \
         you are wrong: complete `{phase_key}` and let its judge pass before \
         advancing. (Reviewer: refine or remove this stanza if the root cause was \
         the gate itself, not the agent.)\n"
    )
}

/// Phase key → branch slug for the self-annealing PR branch. Keeps it within
/// `[a-z0-9-]` so it's a safe git ref.
fn branch_slug(phase_key: &str) -> String {
    let s: String = phase_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("self-annealing/{}", s.trim_matches('-'))
}

/// Collect the candidate phases (failure count ≥ threshold) from session state.
fn candidates(state: &SessionState, threshold: u32) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = state
        .failed_submissions
        .iter()
        .filter(|(_, a)| is_annealing_candidate(a, threshold))
        .map(|(k, a)| (k.clone(), a.count))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Stop-hook entry point.
#[must_use]
pub fn process(
    input: &HookInput,
    ctx: &super::HookContext<'_>,
    state: &SessionState,
) -> HookOutput {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(input, ctx, state)))
        .unwrap_or_else(|_| HookOutput::allow())
}

fn run(input: &HookInput, ctx: &super::HookContext<'_>, state: &SessionState) -> HookOutput {
    let cands = candidates(state, SELF_ANNEAL_FAILURE_THRESHOLD);
    if cands.is_empty() {
        return HookOutput::allow();
    }

    // --- Auto-patch + PR half (armed only) ---
    let armed = ctx.env.var(SELF_ANNEAL_ARM_ENV).as_deref() == Some("1");
    let mut pr_notes: Vec<String> = Vec::new();
    if armed {
        let cwd = input.cwd.as_deref().unwrap_or(".");
        for (phase_key, count) in &cands {
            if let Some(note) = try_auto_patch(ctx, cwd, phase_key, *count) {
                pr_notes.push(note);
            }
        }
    }

    // --- Detect half (always on) ---
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "{} phase gate(s) failed {}+ times this session — candidates for \
         self-annealing (harden the skill so the bypass can't recur):",
        cands.len(),
        SELF_ANNEAL_FAILURE_THRESHOLD
    ));
    for (phase_key, count) in cands.iter().take(5) {
        lines.push(format!("  - `{phase_key}` — {count} failures"));
    }
    if armed {
        if pr_notes.is_empty() {
            lines.push(
                "Auto-patch is armed (SENTINEL_ALLOW_SELF_ANNEAL=1) but no skill file \
                 was eligible/locatable to patch — review and harden manually."
                    .to_string(),
            );
        } else {
            lines.push("Auto-patch (armed) opened the following:".to_string());
            lines.extend(pr_notes.iter().map(|n| format!("  - {n}")));
        }
    } else {
        lines.push(
            "File a `TaskCreate` to harden the failing skill (add an explicit \
             anti-pattern / hard-constraint stanza), or arm SENTINEL_ALLOW_SELF_ANNEAL=1 \
             to let sentinel open a [Self-Annealing] PR for review."
                .to_string(),
        );
    }

    let envelope = HookEnvelope::new("Self-Annealing", HookTier::Warn, lines.join("\n"));
    HookOutput::inject_envelope(HookEvent::Stop, &envelope)
}

/// Attempt the armed auto-patch for one phase. Returns a human-readable note on
/// success, `None` on any guard rejection or IO failure (fail-open). NEVER
/// writes to a protected path (guarded by [`self_anneal_patch_allowed`]) and
/// NEVER commits to the working branch (PR branch + `gh pr create` only).
fn try_auto_patch(
    ctx: &super::HookContext<'_>,
    cwd: &str,
    phase_key: &str,
    count: u32,
) -> Option<String> {
    // Resolve the skill from the "skill:phase" key, then the skill file.
    let skill = phase_key.split(':').next().unwrap_or("");
    if skill.is_empty() {
        return None;
    }
    let skill_path = locate_skill_file(ctx, cwd, skill)?;
    let normalized = skill_path.replace('\\', "/");

    // HARD GATE: never patch a protected path, even armed.
    if !self_anneal_patch_allowed(&normalized, true) {
        return None;
    }

    // PR branch — never the working branch.
    let branch = branch_slug(phase_key);
    let co = ctx
        .process
        .run("git", &["checkout", "-b", &branch], Some(cwd))
        .ok()?;
    if !co.success {
        return None;
    }

    // Append the conservative stanza to the skill file.
    let stanza = anti_pattern_stanza(phase_key, count);
    let existing = ctx
        .fs
        .read_to_string(std::path::Path::new(&skill_path))
        .unwrap_or_default();
    if existing.contains("[Self-Annealing] auto-inserted") {
        // Already annealed for some phase — don't stack duplicates.
        return None;
    }
    let updated = format!("{existing}{stanza}");
    ctx.fs
        .write(std::path::Path::new(&skill_path), updated.as_bytes())
        .ok()?;

    // Commit on the PR branch + open the PR (human reviews; never auto-merged).
    let _ = ctx.process.run("git", &["add", &skill_path], Some(cwd));
    let msg = format!("docs(skill): self-anneal {skill} after {count} {phase_key} gate failures");
    let _ = ctx.process.run("git", &["commit", "-m", &msg], Some(cwd));
    let title = format!("[Self-Annealing] harden {skill}: {phase_key} bypassed {count}x");
    let body = "Auto-prepared by the self_annealing hook. Review the inserted \
                hard-constraint stanza, refine the rule, then merge.";
    let _ = ctx.process.run(
        "gh",
        &["pr", "create", "--title", &title, "--body", body],
        Some(cwd),
    );

    Some(format!(
        "PR branch `{branch}` for `{phase_key}` ({count} failures)"
    ))
}

/// Best-effort location of a skill's SKILL.md. Checks the common marketplace
/// layouts under the home `.claude/skills/<skill>/SKILL.md`. Returns the path
/// string if the file exists.
fn locate_skill_file(ctx: &super::HookContext<'_>, _cwd: &str, skill: &str) -> Option<String> {
    let home = ctx.env.var("HOME").or_else(|| ctx.env.var("USERPROFILE"))?;
    let candidate = format!("{home}/.claude/skills/{skill}/SKILL.md").replace('\\', "/");
    if ctx
        .fs
        .read_to_string(std::path::Path::new(&candidate))
        .is_ok()
    {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn attempts(count: u32) -> SubmissionAttempts {
        SubmissionAttempts {
            count,
            last_failure: Some(Utc::now()),
        }
    }

    #[test]
    fn candidate_threshold() {
        assert!(!is_annealing_candidate(&attempts(0), 3));
        assert!(!is_annealing_candidate(&attempts(2), 3));
        assert!(is_annealing_candidate(&attempts(3), 3));
        assert!(is_annealing_candidate(&attempts(9), 3));
    }

    #[test]
    fn patch_predicate_requires_armed() {
        // Even a perfectly valid skill md is refused when disarmed.
        assert!(!self_anneal_patch_allowed(
            "/home/u/.claude/skills/linear/SKILL.md",
            false
        ));
        assert!(self_anneal_patch_allowed(
            "/home/u/.claude/skills/linear/SKILL.md",
            true
        ));
    }

    #[test]
    fn patch_predicate_never_touches_protected_paths_even_armed() {
        // Phase file.
        assert!(!self_anneal_patch_allowed(
            "/home/u/.claude/skills/linear/phases/claim.md",
            true
        ));
        // Live sentinel config/state.
        assert!(!self_anneal_patch_allowed(
            "/home/u/.claude/sentinel/config/skills/x/SKILL.md",
            true
        ));
        // settings.json.
        assert!(!self_anneal_patch_allowed(
            "/home/u/.claude/settings.json",
            true
        ));
        // Sentinel source.
        assert!(!self_anneal_patch_allowed(
            "/repo/crates/sentinel-application/src/skills/x.md",
            true
        ));
        // Engine binary / cargo bin.
        assert!(!self_anneal_patch_allowed(
            "/home/u/.cargo/bin/sentinel-engine",
            true
        ));
        // Workflow / hook policy.
        assert!(!self_anneal_patch_allowed(
            "/repo/config/workflows.toml",
            true
        ));
        // Non-skill, non-md.
        assert!(!self_anneal_patch_allowed(
            "/home/u/.claude/skills/linear/notes.txt",
            true
        ));
    }

    #[test]
    fn stanza_is_deterministic_and_marked() {
        let s = anti_pattern_stanza("linear:review", 4);
        assert!(s.contains("[Self-Annealing] auto-inserted"));
        assert!(s.contains("linear:review"));
        assert!(s.contains("**4**"));
        assert!(s.contains("not optional"));
    }

    #[test]
    fn branch_slug_is_safe_ref() {
        assert_eq!(branch_slug("linear:review"), "self-annealing/linear-review");
        assert_eq!(branch_slug("a/b:c"), "self-annealing/a-b-c");
    }

    #[test]
    fn detect_half_silent_with_no_failures() {
        let state = SessionState::new("sess-1");
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput::default();
        let out = process(&input, &ctx, &state);
        assert!(out.blocked.is_none());
        // No candidates → no injected envelope.
        let injected = out
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
            .unwrap_or_default();
        assert!(!injected.contains("Self-Annealing"), "{injected}");
    }

    #[test]
    fn detect_half_fires_on_repeated_failures_disarmed() {
        let mut state = SessionState::new("sess-1");
        // 3 failures of the same phase.
        state.record_submission_failure("linear:review");
        state.record_submission_failure("linear:review");
        state.record_submission_failure("linear:review");

        // Disarmed env (stub_ctx has no SENTINEL_ALLOW_SELF_ANNEAL).
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput::default();
        let out = process(&input, &ctx, &state);

        // Never blocks; emits the detect envelope but does NOT auto-patch.
        assert!(out.blocked.is_none());
        let injected = out
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
            .unwrap_or_default();
        assert!(injected.contains("Self-Annealing"), "{injected}");
        assert!(injected.contains("linear:review"), "{injected}");
        // Disarmed → it should advise arming / TaskCreate, not claim a PR.
        assert!(
            injected.contains("TaskCreate") || injected.contains("arm"),
            "{injected}"
        );
        assert!(
            !injected.contains("PR branch"),
            "disarmed must not open a PR: {injected}"
        );
    }
}
