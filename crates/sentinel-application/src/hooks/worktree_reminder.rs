//! Worktree Reminder Hook
//!
//! Runs on UserPromptSubmit. Detects when the user is asking for code changes
//! in a git repository and injects a reminder to use `EnterWorktree` to isolate
//! changes rather than editing directly on the current branch.
//!
//! Only fires when:
//! 1. The cwd is a git repository (has .git dir or file)
//! 2. The session is NOT already inside a worktree
//! 3. The user prompt suggests code changes (edit, fix, update, refactor, etc.)

use std::path::Path;

use regex::Regex;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Detect if the cwd is inside a git repository.
fn is_git_repo(cwd: &str) -> bool {
    let path = Path::new(cwd);
    // Walk up looking for .git (dir or file — worktrees use a .git file)
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// Detect if the cwd is already inside a Claude Code worktree.
/// Claude Code worktrees live under `.claude/worktrees/`.
fn is_inside_worktree(cwd: &str) -> bool {
    let normalized = cwd.replace('\\', "/");
    normalized.contains(".claude/worktrees/")
}

/// Detect if the user prompt suggests code changes.
fn suggests_code_changes(prompt: &str) -> bool {
    let patterns = [
        r"\b(?:edit|fix|update|change|modify|refactor|implement|add|create|write|build|scaffold)\b",
        r"\b(?:bug|feature|patch|upgrade|migrate|port|rewrite|replace|remove|delete)\b",
        r"\b(?:code|function|method|class|struct|module|crate|component|file|test)\b",
    ];
    let lower = prompt.to_lowercase();
    // Need at least one action word match
    patterns
        .iter()
        .any(|p| Regex::new(p).map(|re| re.is_match(&lower)).unwrap_or(false))
}

pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let cwd = match &input.cwd {
        Some(c) => c.as_str(),
        None => return HookOutput::allow(),
    };

    // Skip if not in a git repo
    if !is_git_repo(cwd) {
        return HookOutput::allow();
    }

    // Skip if already inside a worktree
    if is_inside_worktree(cwd) {
        return HookOutput::allow();
    }

    let prompt = match &input.prompt {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    // Only remind when the prompt suggests code changes
    if !suggests_code_changes(prompt) {
        return HookOutput::allow();
    }

    let mut msg = String::from(
        "🟡 [Worktree Reminder] You are in a git repository. \
         Use `EnterWorktree` to create an isolated worktree before making code changes. \
         This is a user preference that applies to ALL repos. \
         While working, use `AskUserQuestion` whenever you hit a decision point or \
         unclear requirement — do not guess.",
    );

    // Attach a list of merged `worktree-*` branches so cleanup is one
    // copy-paste away. Only run git when we know we'll inject — keeps the hot
    // path cheap for prompts that don't trigger the reminder.
    if let Some(repo_root) = ctx.git.repo_root(cwd) {
        let merged_local: Vec<String> = ctx
            .git
            .merged_local_branches(&repo_root, "main")
            .into_iter()
            .filter(|b| b.starts_with("worktree-"))
            .collect();
        if !merged_local.is_empty() {
            let cmds = merged_local
                .iter()
                .map(|b| format!("  git branch -d {b}"))
                .collect::<Vec<_>>()
                .join("\n");
            msg.push_str(&format!(
                "\n\n[Branch Cleanup] {} local `worktree-*` branch(es) merged into main \
                 — safe to delete:\n{cmds}",
                merged_local.len()
            ));
        }
    }

    HookOutput::inject_context(HookEvent::UserPromptSubmit, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggests_code_changes() {
        assert!(suggests_code_changes("fix the bug in auth"));
        assert!(suggests_code_changes("update sentinel code"));
        assert!(suggests_code_changes("refactor the module"));
        assert!(suggests_code_changes("add a new feature"));
        assert!(suggests_code_changes("implement the struct"));
        assert!(suggests_code_changes("create a test file"));
    }

    #[test]
    fn test_does_not_suggest_code_changes() {
        assert!(!suggests_code_changes("hello"));
        assert!(!suggests_code_changes("what time is it"));
        assert!(!suggests_code_changes("list my linear issues"));
    }

    #[test]
    fn test_is_inside_worktree() {
        assert!(is_inside_worktree(
            "C:\\Users\\garys\\.claude\\worktrees\\my-branch"
        ));
        assert!(is_inside_worktree(
            "/home/user/.claude/worktrees/feat-xyz"
        ));
        assert!(!is_inside_worktree(
            "C:\\Users\\garys\\Documents\\GitHub\\sentinel"
        ));
    }

    #[test]
    fn test_process_no_cwd() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_not_git_repo() {
        let input = HookInput {
            cwd: Some("/tmp/not-a-repo".to_string()),
            prompt: Some("fix the code".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }
}
