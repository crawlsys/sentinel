//! Declarative auto-cron rule engine.
//!
//! `PostToolUse` hook that matches the current tool call against the declarative
//! ruleset ([`sentinel_infrastructure::autocron_config`]) and injects a
//! `CronCreate`/loop SUGGESTION into model context. Consolidates the two
//! previously-hardcoded cron emitters (`pr_auto_monitor`'s `gh pr create` branch,
//! `linear_lifecycle`'s `state_id` branch) into one data-driven hook and closes
//! the deploy-started / PR-push / stale-task / CI-run / idempotency gaps.
//!
//! Mechanism is byte-identical to the hooks it replaces: every emission is
//! `HookOutput::inject_context(PostToolUse, "…CronCreate(...)…")` →
//! `additionalContext` only. Sentinel owns no cron store — the agent reads the
//! suggestion and calls the `CronCreate`/`CronDelete`/loop client tools itself.
//! The novelty is *declarative operator-editable rules + per-session dedupe +
//! mandatory self-delete/safety-cap rendering + argv-tokenized classification*.

use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use crate::autocron_config::{self, AutocronRule};

use super::pr_auto_monitor::extract_pr_from_result;

/// Process a `PostToolUse` event against the declarative autocron rules.
/// Gated to nothing here — each rule declares its own `tool`, so this handles
/// Bash, MCP tools (e.g. `mcp__linear__update_issue`), and `TaskUpdate` alike.
pub fn process(input: &HookInput) -> HookOutput {
    let Some(tool) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };

    let rules = autocron_config::load();
    let haystack = haystack_for(input);

    for rule in rules.iter().filter(|r| r.tool == tool) {
        // Cheap literal guard before the regex.
        if rule
            .skip_tokens
            .iter()
            .any(|t| haystack.contains(t.as_str()))
        {
            continue;
        }

        // Classify: regex (default) or the git_push_branch argv tokenizer.
        let caps: HashMap<String, String> = match rule.match_kind.as_deref() {
            Some("git_push_branch") => match classify_push_branch(&haystack) {
                Some(branch) => {
                    let mut m = HashMap::new();
                    m.insert("branch".to_string(), branch);
                    m
                }
                None => continue, // main/master or no explicit refspec → skip
            },
            _ => match regex_captures(&rule.match_re, &haystack) {
                Some(m) => m,
                None => continue,
            },
        };

        // Exclude guard.
        if let Some(ex) = &rule.exclude {
            if Regex::new(ex)
                .map(|r| r.is_match(&haystack))
                .unwrap_or(false)
            {
                continue;
            }
        }

        // Dedupe (one suggestion per rule+key per session).
        let dedupe_id = resolve_dedupe_id(rule, &caps);
        if already_suggested(input, &dedupe_id) {
            return HookOutput::allow();
        }

        let body = render_template(&rule.prompt_template, &caps, input);
        let prefix = if rule.authority {
            "[Sentinel-Authority] [Autocron] "
        } else {
            "[Autocron] "
        };
        mark_suggested(input, &dedupe_id);
        return HookOutput::inject_context(HookEvent::PostToolUse, format!("{prefix}{body}"));
    }

    HookOutput::allow()
}

/// The text a rule matches against: a Bash command's `command`, otherwise the
/// flattened `tool_input` JSON (so MCP/`TaskUpdate` rules can match on fields).
fn haystack_for(input: &HookInput) -> String {
    if input.tool_name.as_deref() == Some("Bash") {
        input
            .tool_input
            .as_ref()
            .and_then(|v| v.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        input
            .tool_input
            .as_ref()
            .map(std::string::ToString::to_string)
            .unwrap_or_default()
    }
}

/// Compile `pattern` and return named captures (name → value) on a match.
/// An empty map still signals "matched" (vs `None` for no match).
fn regex_captures(pattern: &str, haystack: &str) -> Option<HashMap<String, String>> {
    let re = Regex::new(pattern).ok()?;
    let caps = re.captures(haystack)?;
    let mut map = HashMap::new();
    for name in re.capture_names().flatten() {
        if let Some(m) = caps.name(name) {
            map.insert(name.to_string(), m.as_str().to_string());
        }
    }
    Some(map)
}

/// Argv-tokenized `git push` branch classifier (Design B). Returns the explicit
/// pushed branch when it is NOT main/master, else `None`. Fixes the blunt
/// `!contains("main")` bug that wrongly skipped `git push origin feat/main-menu`.
fn classify_push_branch(cmd: &str) -> Option<String> {
    // Only consider real `git push` invocations.
    if !cmd.contains("git") || !cmd.contains("push") {
        return None;
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let push_pos = tokens.iter().position(|t| *t == "push")?;
    // Collect positional args after `push`, skipping flags and their values.
    let mut positionals: Vec<&str> = Vec::new();
    let mut i = push_pos + 1;
    while i < tokens.len() {
        let t = tokens[i];
        if t.starts_with('-') {
            // `-u`/`--set-upstream` etc.; flags with values are rare for push and
            // their values are still positional-looking, so just skip the flag.
            i += 1;
            continue;
        }
        positionals.push(t);
        i += 1;
    }
    // Shapes: `git push` (none) → no explicit branch; `git push origin <branch>`
    // → branch is the 2nd positional; `git push origin` → no branch.
    let branch = match positionals.as_slice() {
        [_remote, branch, ..] => *branch,
        _ => return None,
    };
    // Strip a `src:dst` refspec to its destination-ish leaf for display.
    let branch = branch.split(':').next_back().unwrap_or(branch);
    if branch == "main" || branch == "master" || branch.is_empty() {
        return None;
    }
    Some(branch.to_string())
}

/// Resolve the dedupe id `{rule.id}:{value}` where value is the `dedupe_key`
/// capture if present and resolvable, else the rule id alone.
fn resolve_dedupe_id(rule: &AutocronRule, caps: &HashMap<String, String>) -> String {
    match rule.dedupe_key.as_deref().and_then(|k| caps.get(k)) {
        Some(v) => format!("{}:{}", rule.id, v),
        None => rule.id.clone(),
    }
}

/// Render `{capture}` placeholders + builtins `{pr_ref}` `{cwd}` `{branch}`
/// `{run_id}`. Unresolved placeholders are left literal (debuggable).
fn render_template(template: &str, caps: &HashMap<String, String>, input: &HookInput) -> String {
    let mut out = template.to_string();

    // Named captures first (a rule capture named `branch` wins over the builtin).
    for (k, v) in caps {
        out = out.replace(&format!("{{{k}}}"), v);
    }

    // Builtins (only substitute if still present and not already from a capture).
    if out.contains("{pr_ref}") {
        let pr = extract_pr_from_result(input).unwrap_or_else(|| "the new PR".to_string());
        out = out.replace("{pr_ref}", &pr);
    }
    if out.contains("{cwd}") {
        let cwd = input.cwd.clone().unwrap_or_else(|| ".".to_string());
        out = out.replace("{cwd}", &cwd);
    }
    if out.contains("{branch}") {
        // Fall back to the cwd's basename if no branch capture supplied one.
        let branch = caps
            .get("branch")
            .cloned()
            .unwrap_or_else(|| "this branch".to_string());
        out = out.replace("{branch}", &branch);
    }
    if out.contains("{run_id}") {
        out = out.replace("{run_id}", "the run");
    }
    out
}

// --- Per-session dedupe ledger (the only new state; NOT a cron store) ---
//
// `~/.claude/sentinel/state/autocron-suggested-<session>.jsonl`, one dedupe id
// per line. Per-session ⇒ auto-expires, no GC, a new session legitimately
// re-arms. Fail-open: an unreadable/unwritable ledger at worst allows one
// duplicate suggestion, never a crash.

fn ledger_path(input: &HookInput) -> PathBuf {
    let session = input
        .session_id
        .clone()
        .unwrap_or_else(|| "nosession".to_string());
    crate::paths::claude_dir()
        .join("sentinel")
        .join("state")
        .join(format!("autocron-suggested-{session}.jsonl"))
}

fn already_suggested(input: &HookInput, dedupe_id: &str) -> bool {
    let path = ledger_path(input);
    match std::fs::read_to_string(&path) {
        Ok(content) => content.lines().any(|line| line.trim() == dedupe_id),
        Err(_) => false,
    }
}

fn mark_suggested(input: &HookInput, dedupe_id: &str) {
    let path = ledger_path(input);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append-only; ignore errors (fail-open).
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{dedupe_id}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({ "command": cmd })),
            // Unique session per command so the dedupe ledger doesn't bleed across
            // tests. The ledger persists on disk between `cargo test` runs, so we
            // ALSO clean it (via `fresh`) before any emit assertion.
            session_id: Some(format!(
                "test-{}",
                cmd.len() * 7 + cmd.bytes().map(|b| b as usize).sum::<usize>()
            )),
            ..Default::default()
        }
    }

    /// Wipe the dedupe ledger for `input` so an emit-test is order-independent and
    /// survives a stale ledger from a prior `cargo test` run.
    fn fresh(input: &HookInput) -> HookOutput {
        let _ = std::fs::remove_file(ledger_path(input));
        process(input)
    }

    fn ctx_of(out: &HookOutput) -> String {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
            .unwrap_or_default()
    }

    #[test]
    fn gh_pr_create_emits_pr_cron() {
        let mut input = bash("gh pr create --title x --body y");
        input.tool_result = Some(serde_json::json!("https://github.com/o/r/pull/42"));
        let out = fresh(&input);
        let c = ctx_of(&out);
        assert!(c.contains("CronCreate"), "should emit a cron: {c}");
        assert!(c.contains("#42"), "pr_ref should resolve to #42: {c}");
        assert!(c.contains("*/5 * * * *"));
        assert!(c.contains("CronDelete"), "terminal rule must self-delete");
    }

    #[test]
    fn push_to_feature_branch_named_main_menu_is_not_skipped() {
        // Regression for the `!contains("main")` bug.
        let out = fresh(&bash("git push origin feat/main-menu"));
        let c = ctx_of(&out);
        assert!(
            c.contains("CronCreate"),
            "feat/main-menu push must arm a monitor: {c}"
        );
        assert!(c.contains("feat/main-menu"), "branch must render: {c}");
    }

    #[test]
    fn push_to_main_is_skipped() {
        let out = process(&bash("git push origin main"));
        assert!(
            ctx_of(&out).is_empty(),
            "pushing main must not arm a PR-branch monitor"
        );
    }

    #[test]
    fn wrangler_deploy_emits_authoritative_deploy_cron() {
        let out = fresh(&bash("wrangler deploy --env staging"));
        let c = ctx_of(&out);
        assert!(
            c.contains("[Sentinel-Authority]"),
            "deploy rule is authoritative: {c}"
        );
        assert!(c.contains("*/2 * * * *"));
        assert!(c.contains("CronDelete"));
    }

    #[test]
    fn dry_run_deploy_is_skipped() {
        let out = process(&bash("wrangler deploy --dry-run"));
        assert!(
            ctx_of(&out).is_empty(),
            "--dry-run must be suppressed by skip_tokens"
        );
    }

    #[test]
    fn echoed_command_is_skipped() {
        let out = process(&bash("echo gh pr create"));
        assert!(ctx_of(&out).is_empty(), "echoed command must be suppressed");
    }

    #[test]
    fn linear_state_change_emits_cron() {
        let input = HookInput {
            tool_name: Some("mcp__linear__update_issue".to_string()),
            tool_input: Some(serde_json::json!({ "id": "X", "state_id": "abc-123" })),
            session_id: Some("test-linear".to_string()),
            ..Default::default()
        };
        let c = ctx_of(&fresh(&input));
        assert!(
            c.contains("CronCreate"),
            "linear state change emits a cron: {c}"
        );
        assert!(c.contains("abc-123"), "issue capture renders: {c}");
        assert!(c.contains("47 * * * *"));
    }

    #[test]
    fn linear_update_without_state_id_emits_nothing() {
        let input = HookInput {
            tool_name: Some("mcp__linear__update_issue".to_string()),
            tool_input: Some(serde_json::json!({ "id": "X", "title": "renamed" })),
            session_id: Some("test-linear-2".to_string()),
            ..Default::default()
        };
        assert!(ctx_of(&process(&input)).is_empty());
    }

    #[test]
    fn task_update_in_progress_emits_stale_watch() {
        let input = HookInput {
            tool_name: Some("TaskUpdate".to_string()),
            tool_input: Some(serde_json::json!({ "taskId": "5", "status": "in_progress" })),
            session_id: Some("test-task".to_string()),
            ..Default::default()
        };
        let c = ctx_of(&fresh(&input));
        assert!(
            c.contains("CronCreate"),
            "in_progress task arms a stale watch: {c}"
        );
        assert!(c.contains("*/30 * * * *"));
    }

    #[test]
    fn dedupe_suppresses_second_identical_call() {
        let session = "test-dedupe-unique";
        let make = || HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({ "command": "wrangler deploy --env prod" })),
            session_id: Some(session.to_string()),
            ..Default::default()
        };
        // Clean any prior ledger from a previous test run.
        let _ = std::fs::remove_file(ledger_path(&make()));
        let first = ctx_of(&process(&make()));
        assert!(first.contains("CronCreate"), "first call arms: {first}");
        let second = ctx_of(&process(&make()));
        assert!(
            second.is_empty(),
            "second identical call is deduped: {second}"
        );
        let _ = std::fs::remove_file(ledger_path(&make()));
    }

    #[test]
    fn classify_push_branch_handles_shapes() {
        assert_eq!(
            classify_push_branch("git push origin feat/x").as_deref(),
            Some("feat/x")
        );
        assert_eq!(
            classify_push_branch("git push -u origin feat/y").as_deref(),
            Some("feat/y")
        );
        assert_eq!(classify_push_branch("git push origin main"), None);
        assert_eq!(classify_push_branch("git push origin"), None);
        assert_eq!(classify_push_branch("git push"), None);
        assert_eq!(
            classify_push_branch("git push origin HEAD:feat/z").as_deref(),
            Some("feat/z")
        );
    }
}
