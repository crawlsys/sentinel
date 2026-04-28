//! Task Coverage Check — Stop hook
//!
//! When Claude finishes responding, check if there are uncommitted file
//! changes but no in_progress task. Warns about untracked work so nothing
//! slips through without a task.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::HookContext;

pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Check for uncommitted changes via git
    let has_changes = ctx.git.has_uncommitted_changes(cwd).unwrap_or(false);

    if !has_changes {
        return HookOutput::allow();
    }

    // Check if there's an active task marker
    let session_id = match input.session_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return HookOutput::allow(),
    };

    let active_marker = std::env::temp_dir().join(format!("claude-task-active-{session_id}"));
    if ctx.fs.exists(&active_marker) {
        return HookOutput::allow();
    }

    let context = "[Task Coverage] WARNING: Uncommitted file changes detected but no task is \
         in_progress. All work should be tracked as a task. Create a task with `TaskCreate` \
         and mark it `in_progress` with `TaskUpdate` to track this work.";

    HookOutput::inject_context(HookEvent::Stop, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allows_when_no_changes() {
        let input = HookInput {
            cwd: Some("/tmp/test".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }
}
