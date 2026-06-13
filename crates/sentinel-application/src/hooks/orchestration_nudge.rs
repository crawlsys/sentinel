//! Orchestration Nudge Hook
//!
//! `UserPromptSubmit` hook that nudges three orchestration patterns based on
//! heuristics about the prompt text:
//!
//! 1. **Agent teams** — when the prompt describes 3+ independent parallel
//!    subtasks, suggest `TeamCreate` rather than serial subagents.
//! 2. **Subagents / Explore agent** — when the prompt suggests broad
//!    exploration ("find all", "search everywhere", "audit") and we're not
//!    already in a subagent, suggest `Agent(subagent_type: "Explore")` to
//!    protect the main context.
//! 3. **Skills** — when the prompt looks like it matches a skill activation
//!    trigger but `skill_router` already fired, we don't re-nudge. When
//!    `skill_router` reported "No skill matched" but the prompt looks like
//!    skill work (multi-step implementation, structured process), we
//!    suggest invoking the relevant skill via `Skill(skill: "<name>")`.
//!
//! This is a soft nudge — injects context, never blocks.

use regex::Regex;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Regex compiled lazily — sentinel is single-threaded per invocation so
/// `OnceLock` isn't needed; a fresh compile per call is cheap for these.
fn parallel_signal(prompt: &str) -> bool {
    let patterns = [
        r"\bin parallel\b",
        r"\bconcurrently\b",
        r"\bat the same time\b",
        r"\b(\d+)\s+(tasks|things|items|steps|todos)\b", // "5 tasks"
        r"\b(all|each) of (the|these) \w+",              // "all of these bugs"
        r"\bacross (all|every|\d+)",                     // "across all repos"
    ];
    let lower = prompt.to_lowercase();
    patterns
        .iter()
        .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&lower)))
}

fn broad_exploration_signal(prompt: &str) -> bool {
    let patterns = [
        r"\bfind all\b",
        r"\bsearch (the |)(whole |entire |)codebase\b",
        r"\baudit (the|this|all)\b",
        r"\bwhere is .* (used|defined|called|referenced)\b",
        r"\bcheck (all|every)\b",
        r"\bacross (all|the|every) files?\b",
        r"\bdepend(s|encies) on\b",
    ];
    let lower = prompt.to_lowercase();
    patterns
        .iter()
        .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&lower)))
}

fn multi_step_implementation_signal(prompt: &str) -> bool {
    // Words that typically precede a multi-step task that a skill would handle.
    let patterns = [
        r"\b(implement|build|refactor|migrate|deploy|ship|release)\b",
        r"\b(fix) (all|every|the|this) \w+",
        r"\bend[- ]to[- ]end\b",
        r"\bfull (stack|flow|pipeline|workflow)\b",
    ];
    let lower = prompt.to_lowercase();
    patterns
        .iter()
        .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&lower)))
}

/// Large fan-out orchestration that the native `Workflow` tool handles far
/// better than `/loop` driving subagents one turn at a time: codebase-wide
/// sweeps, big migrations, multi-angle research, adversarial judge panels. The
/// `Workflow` script holds the loop/branching itself and returns only the final
/// answer, so the main context isn't spent coordinating dozens of agents.
fn workflow_orchestration_signal(prompt: &str) -> bool {
    let patterns = [
        r"\b(audit|sweep|migrate|review) (every|all|the (whole|entire))\b",
        r"\bacross (all|every|the (whole|entire)).*(repo|file|endpoint|module|ticket)",
        r"\b(\d{2,}|dozens?|hundreds?) of (files?|agents?|tickets?|endpoints?|repos?)\b",
        r"\bjudge panel\b",
        r"\bcross[- ]check\b.*\b(sources?|claims?|findings?)\b",
    ];
    let lower = prompt.to_lowercase();
    patterns
        .iter()
        .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&lower)))
}

/// A request to `/loop` / poll / babysit something that ALREADY notifies the
/// session on completion (a `Workflow`, a background subagent, a CI/deploy that
/// pushes via Channels). Anthropic fixed exactly this for native `/loop`
/// (changelog v2.1.141 / v2.1.147 — stopped scheduling redundant wakeups for
/// background tasks that already notify). The completion notification
/// re-invokes the session, so an interval poll is wasted tokens; prefer the
/// notification, Channels, or the Monitor tool. Requires BOTH a poll/wait verb
/// AND a self-notifying target — plain polling of an external service that does
/// NOT push to the session is legitimate and must not trip.
fn redundant_poll_signal(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    let poll_verb = [
        r"\b(/loop|loop|poll|babysit|keep checking|watch|wait (for|until))\b",
        r"\bcheck (every|back)\b",
    ]
    .iter()
    .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&lower)));

    let self_notifying_target = [
        r"\bworkflow\b",
        r"\bbackground (agent|subagent|task|job)\b",
        r"\bagent (to )?(finish|complete|notif)",
        r"\b(deploy|build|ci) (that|which) (notif|push)",
    ]
    .iter()
    .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&lower)));

    poll_verb && self_notifying_target
}

/// True if we're executing inside a subagent — we don't want to nudge
/// subagents to spawn more subagents (could recurse).
fn is_in_subagent(input: &HookInput) -> bool {
    input
        .extra
        .get("agent_type")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty() && s != "main")
        || input.extra.get("parent_session_id").is_some()
}

pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // Skip inside subagents — already delegated work.
    if is_in_subagent(input) {
        return HookOutput::allow();
    }

    let prompt = match &input.prompt {
        Some(p) if !p.is_empty() => p,
        _ => return HookOutput::allow(),
    };

    // Skip very short prompts — nothing to orchestrate.
    if prompt.len() < 40 {
        return HookOutput::allow();
    }

    let mut nudges: Vec<&str> = Vec::new();

    if parallel_signal(prompt) {
        nudges.push(
            "- **Agent Teams**: the prompt describes multiple independent items. \
             Consider `TeamCreate(team_name: \"<name>\")` + 3-5 teammates over \
             serial Task() calls. Teammates share a task list, communicate via \
             SendMessage, and each run in their own context window — much better \
             than one agent sequentially chewing through 5 items in yours.",
        );
    }

    if broad_exploration_signal(prompt) {
        nudges.push(
            "- **Explore subagent**: the prompt calls for broad codebase discovery. \
             Use `Agent(subagent_type: \"Explore\", prompt: \"...\")` instead of \
             Glob+Grep chains in your own context — keeps your working memory free \
             for the actual work.",
        );
    }

    if multi_step_implementation_signal(prompt) {
        nudges.push(
            "- **Skills**: multi-step implementation work typically has a matching \
             skill (e.g. `execute`, `refactor`, `migrate`, `deploy`). If the skill \
             router didn't already route, consider invoking `Skill(skill: \"<name>\")` \
             explicitly — skills enforce phase workflows and bring pre-built agent \
             orchestration patterns.",
        );
    }

    if workflow_orchestration_signal(prompt) {
        nudges.push(
            "- **Workflow (native)**: this is large fan-out — a codebase sweep, big \
             migration, multi-angle research, or judge panel. Reach for the native \
             `Workflow` tool, not `/loop` driving subagents one turn at a time. The \
             workflow script holds the loop and branching itself and returns only the \
             final answer, so your context isn't spent coordinating dozens of agents \
             (up to 16 concurrent / 1000 total per run).",
        );
    }

    if redundant_poll_signal(prompt) {
        nudges.push(
            "- **Don't double-drive a self-notifying task**: you're about to poll/`/loop` \
             something (a Workflow, a background subagent, a CI/deploy that pushes) that \
             ALREADY notifies the session on completion. The notification re-invokes you \
             — an interval poll is wasted tokens. This mirrors Anthropic's own native \
             fix (changelog v2.1.141). Prefer the completion notification, **Channels** \
             (event → session), or the **Monitor** tool over a poll. For \
             \"don't stop until X is true\", use `/goal` rather than a fixed-interval loop.",
        );
    }

    if nudges.is_empty() {
        return HookOutput::allow();
    }

    let context = format!(
        "🟡 [Orchestration Nudge] Consider these orchestration patterns for this task:\n{}",
        nudges.join("\n")
    );

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_input(text: &str) -> HookInput {
        HookInput {
            prompt: Some(text.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_parallel_signal_triggers_team_nudge() {
        assert!(parallel_signal(
            "fix 5 bugs in parallel across the frontend"
        ));
        assert!(parallel_signal("handle all of these issues concurrently"));
        assert!(parallel_signal("audit 7 repos at the same time"));
        assert!(!parallel_signal("fix a single bug"));
    }

    #[test]
    fn test_broad_exploration_signal() {
        assert!(broad_exploration_signal(
            "find all usages of the old API across every file"
        ));
        assert!(broad_exploration_signal("audit the codebase for dead code"));
        assert!(!broad_exploration_signal("edit src/main.rs line 42"));
    }

    #[test]
    fn test_multi_step_implementation_signal() {
        assert!(multi_step_implementation_signal(
            "implement the new auth flow end to end"
        ));
        assert!(multi_step_implementation_signal(
            "refactor the billing module to use the new event bus"
        ));
        assert!(!multi_step_implementation_signal("what is 2+2"));
    }

    #[test]
    fn test_injects_all_three_when_all_match() {
        let input = prompt_input(
            "implement the new billing system end to end: find all pricing \
             references across the codebase, then refactor 6 handlers in parallel",
        );
        let ctx = super::super::test_support::stub_ctx();
        let out = process(&input, &ctx);
        let injected = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .unwrap_or_default();
        assert!(
            injected.contains("Agent Teams"),
            "missing team nudge: {injected}"
        );
        assert!(
            injected.contains("Explore subagent"),
            "missing subagent nudge: {injected}"
        );
        assert!(
            injected.contains("Skills"),
            "missing skill nudge: {injected}"
        );
    }

    #[test]
    fn test_workflow_orchestration_signal() {
        assert!(workflow_orchestration_signal(
            "audit every endpoint under src/routes for missing auth checks"
        ));
        assert!(workflow_orchestration_signal(
            "migrate all the files across the whole repo from Solid to React"
        ));
        assert!(workflow_orchestration_signal(
            "run a judge panel on these three design options"
        ));
        assert!(workflow_orchestration_signal(
            "cross-check the claims in this report against sources"
        ));
        // Small/normal work should NOT trip the workflow nudge.
        assert!(!workflow_orchestration_signal("fix the bug in auth.rs"));
        assert!(!workflow_orchestration_signal("review my last commit"));
    }

    #[test]
    fn test_redundant_poll_signal_requires_both_verb_and_target() {
        // poll verb + self-notifying target → trips.
        assert!(redundant_poll_signal(
            "keep checking the workflow and tell me when it's done"
        ));
        assert!(redundant_poll_signal(
            "/loop every 2m to wait for the background agent to finish"
        ));
        assert!(redundant_poll_signal(
            "poll the deploy that pushes a notification when ready"
        ));
        // poll verb but EXTERNAL target that does NOT self-notify → legitimate,
        // must not trip (false-positive guard).
        assert!(!redundant_poll_signal(
            "poll the upstream vendor API every 5 minutes for new orders"
        ));
        assert!(!redundant_poll_signal("watch the stock price"));
        // self-notifying target but no poll verb → not a double-drive.
        assert!(!redundant_poll_signal(
            "dispatch a workflow to audit the endpoints"
        ));
    }

    #[test]
    fn test_workflow_and_poll_nudges_inject() {
        let ctx = super::super::test_support::stub_ctx();
        let wf = process(
            &prompt_input("audit every endpoint across the whole repo for auth bugs"),
            &ctx,
        );
        let wf_ctx = wf
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .unwrap_or_default();
        assert!(wf_ctx.contains("Workflow (native)"), "missing: {wf_ctx}");

        let poll = process(
            &prompt_input("just keep checking the workflow until it finishes and ping me"),
            &ctx,
        );
        let poll_ctx = poll
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .unwrap_or_default();
        assert!(
            poll_ctx.contains("double-drive"),
            "missing poll nudge: {poll_ctx}"
        );
    }

    #[test]
    fn test_no_nudge_for_trivial_prompt() {
        let ctx = super::super::test_support::stub_ctx();
        let out = process(&prompt_input("hi"), &ctx);
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn test_no_nudge_inside_subagent() {
        let mut input =
            prompt_input("implement the auth flow end to end, find all references in parallel");
        input.extra.insert(
            "agent_type".to_string(),
            serde_json::Value::String("Explore".to_string()),
        );
        let ctx = super::super::test_support::stub_ctx();
        let out = process(&input, &ctx);
        assert!(
            out.hook_specific_output.is_none(),
            "subagents should not recurse into more subagent nudges"
        );
    }

    #[test]
    fn test_no_nudge_for_empty_prompt() {
        let ctx = super::super::test_support::stub_ctx();
        let out = process(&HookInput::default(), &ctx);
        assert!(out.hook_specific_output.is_none());
    }
}
