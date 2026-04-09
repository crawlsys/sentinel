//! PostCompact hook — restore critical state after context compaction
//!
//! Called after compaction completes. Receives compact_summary.
//! Can inject additionalContext to restore critical information.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::HookContext;

/// Process PostCompact event
///
/// Restores active skill context and workflow state that may have been
/// lost during compaction.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let trigger = input
        .extra
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(trigger, "Post-compaction state restoration");

    // Read active skill from session state
    let active_skill = ctx
        .fs
        .home_dir()
        .and_then(|home| {
            let session_id = input.session_id.as_deref()?;
            let state_path = home
                .join(".claude")
                .join("sentinel")
                .join("state")
                .join(format!("{session_id}.json"));
            let content = ctx.fs.read_to_string(&state_path).ok()?;
            let state: serde_json::Value = serde_json::from_str(&content).ok()?;
            state
                .get("active_skill")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    if let Some(skill) = &active_skill {
        let context = format!(
            "[Post-Compact Recovery] Context was compacted ({trigger}).\n\
             Active skill: {skill}. Reload phase files if needed.\n\
             Use Read(\"~/.claude/skills/{skill}/SKILL.md\") to restore context.",
        );
        HookOutput::inject_context(HookEvent::PostCompact, &context)
    } else {
        HookOutput::allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::FileSystemPort;
    use std::path::{Path, PathBuf};

    struct MockFs;

    impl FileSystemPort for MockFs {
        fn home_dir(&self) -> Option<PathBuf> { Some(PathBuf::from("/mock/home")) }
        fn read_to_string(&self, _path: &Path) -> anyhow::Result<String> { anyhow::bail!("not found") }
        fn write(&self, _path: &Path, _content: &[u8]) -> anyhow::Result<()> { Ok(()) }
        fn create_dir_all(&self, _path: &Path) -> anyhow::Result<()> { Ok(()) }
        fn read_dir(&self, _path: &Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
        fn exists(&self, _path: &Path) -> bool { false }
        fn is_dir(&self, _path: &Path) -> bool { false }
        fn metadata(&self, _path: &Path) -> anyhow::Result<std::fs::Metadata> { anyhow::bail!("no") }
        fn append(&self, _path: &Path, _content: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

    struct StubGit;
    impl crate::hooks::GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> { Ok(vec![]) }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> { Ok("main".into()) }
        fn is_worktree(&self, _: &str) -> bool { false }
    }

    #[test]
    fn test_post_compact_without_skill() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput::default();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
