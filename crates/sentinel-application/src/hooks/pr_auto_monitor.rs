//! PR Auto-Monitor
//!
//! PostToolUse hook that detects PR-related git operations and injects
//! CronCreate instructions for automated monitoring.
//!
//! Detects:
//! - `gh pr create` → monitor CI, reviews, conflicts every 5 min
//! - `git push` to a branch (not main) → check CI results
//! - `git merge` to main → verify push + changelog

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process a PostToolUse Bash event for PR-related commands.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    // Detect `gh pr create` — PR was just created
    if cmd.contains("gh pr create") {
        // Extract PR number from tool_result if available
        let pr_ref = extract_pr_from_result(input).unwrap_or_else(|| "the new PR".to_string());

        return HookOutput::inject_context(
            HookEvent::PostToolUse,
            format!(
                "[PR Auto-Monitor] PR created ({pr_ref}). \
                 Create a monitoring loop:\n\
                 ```\n\
                 CronCreate(cron: \"*/5 * * * *\", recurring: true,\n\
                   prompt: \"Check PR {pr_ref}: run `gh pr checks` for CI status, \
                 `gh pr view --comments` for new reviews. If CI fails, diagnose. \
                 If new comments, summarize. If all green + approved, tell Gary it's ready to merge.\")\n\
                 ```"
            ),
        );
    }

    // Detect `git push` to a non-main branch — likely pushing to PR
    if cmd.contains("git push") && !cmd.contains("main") && !cmd.contains("master") {
        return HookOutput::inject_context(
            HookEvent::PostToolUse,
            "[PR Auto-Monitor] Push detected. If this branch has an open PR, \
             check CI status in ~2 minutes with `gh pr checks`."
                .to_string(),
        );
    }

    // Detect merge to main — verify push + changelog. When the branch being
    // merged is named `worktree-*`, surface the exact cleanup commands so
    // the orphaned local + remote refs don't pile up the way they did before.
    if (cmd.contains("git merge") && (cmd.contains("main") || cmd.contains("--no-edit")))
        || cmd.contains("git merge --no-edit")
    {
        let mut msg = String::from(
            "[PR Auto-Monitor] Merge to main detected. Verify:\n\
             1. Push to remote: `git push`\n\
             2. Check CHANGELOG.md was updated\n\
             3. Clean up: `ExitWorktree(action: \"remove\")` for the active worktree, \
             `git branch -d <branch>` for the merged local branch, and \
             `git push origin --delete <branch>` if the branch was pushed to origin",
        );
        if let Some(branch) = extract_worktree_branch_name(cmd) {
            msg.push_str(&format!(
                "\n\nMerged branch: `{branch}` — run:\n  \
                 git branch -d {branch}\n  \
                 git push origin --delete {branch}"
            ));
        }
        return HookOutput::inject_context(HookEvent::PostToolUse, msg);
    }

    HookOutput::allow()
}

/// Pull the first `worktree-*` token out of a `git merge ...` command so the
/// reminder can name the exact branch the user just merged. Returns `None`
/// when the merge wasn't of a worktree branch (e.g. plain `git merge main`).
fn extract_worktree_branch_name(cmd: &str) -> Option<String> {
    cmd.split_whitespace()
        .find(|tok| tok.starts_with("worktree-"))
        .map(str::to_string)
}

fn extract_bash_command(input: &HookInput) -> Option<&str> {
    input.tool_input.as_ref()?.get("command")?.as_str()
}

/// Try to extract a PR number or URL from the tool result.
fn extract_pr_from_result(input: &HookInput) -> Option<String> {
    let result = input.tool_result.as_ref()?;
    let text = result
        .as_str()
        .or_else(|| result.get("content").and_then(|c| c.as_str()))?;

    // Look for PR URL pattern
    if let Some(pos) = text.find("/pull/") {
        let after = &text[pos + 6..];
        let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !num.is_empty() {
            return Some(format!("#{num}"));
        }
    }

    // Look for "Created pull request #N"
    if let Some(pos) = text.find('#') {
        let after = &text[pos + 1..];
        let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !num.is_empty() {
            return Some(format!("#{num}"));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": cmd})),
            ..Default::default()
        }
    }

    #[test]
    fn test_detects_gh_pr_create() {
        let output = process(&bash_input("gh pr create --title 'Fix bug' --body 'stuff'"));
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("PR Auto-Monitor"));
        assert!(ctx.unwrap().contains("CronCreate"));
    }

    #[test]
    fn test_detects_git_push_non_main() {
        let output = process(&bash_input("git push -u origin feat/my-branch"));
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("PR Auto-Monitor"));
    }

    #[test]
    fn test_detects_merge_to_main() {
        let output = process(&bash_input("git merge worktree-feat+thing --no-edit"));
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("Merge to main"));
    }

    #[test]
    fn test_ignores_git_push_main() {
        let output = process(&bash_input("git push origin main"));
        // Push to main is fine — no monitor needed
        assert!(
            output.hook_specific_output.is_none()
                || output
                    .hook_specific_output
                    .as_ref()
                    .and_then(|h| h.additional_context.as_deref())
                    .map(|c| !c.contains("PR Auto-Monitor"))
                    .unwrap_or(true)
        );
    }

    #[test]
    fn test_ignores_non_git_commands() {
        assert!(process(&bash_input("cargo test"))
            .hook_specific_output
            .is_none());
        assert!(process(&bash_input("ls -la"))
            .hook_specific_output
            .is_none());
    }

    #[test]
    fn test_ignores_no_tool_input() {
        assert!(process(&HookInput::default())
            .hook_specific_output
            .is_none());
    }

    #[test]
    fn test_extract_pr_from_url() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "gh pr create"})),
            tool_result: Some(serde_json::json!("https://github.com/org/repo/pull/42")),
            ..Default::default()
        };
        assert_eq!(extract_pr_from_result(&input), Some("#42".to_string()));
    }
}
