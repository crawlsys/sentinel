//! Git Hygiene Gate
//!
//! Blocks Edit/Write tools when uncommitted changes exceed thresholds.
//! Encourages small, frequent commits.

use sentinel_domain::events::{HookInput, HookOutput};

use super::GitStatusPort;

const MAX_UNCOMMITTED_FILES: usize = 10;

/// Process a git-hygiene hook event.
///
/// Accepts a `GitStatusPort` implementor so the application layer
/// stays decoupled from the infrastructure layer.
pub fn process(input: &HookInput, git: &dyn GitStatusPort) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return HookOutput::allow(),
    };

    // Only gate Edit and Write
    if tool != "Edit" && tool != "Write" {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Check for uncommitted changes
    match git.has_uncommitted_changes(cwd) {
        Ok(true) => {
            // Count changed files
            match git.changed_files(cwd) {
                Ok(files) if files.len() > MAX_UNCOMMITTED_FILES => HookOutput::block(format!(
                    "Git hygiene: {} uncommitted files (threshold: {}). \
                     Commit your changes before making more edits.\n\
                     Changed files: {}",
                    files.len(),
                    MAX_UNCOMMITTED_FILES,
                    files
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                _ => HookOutput::allow(),
            }
        }
        _ => HookOutput::allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub GitStatusPort for testing
    struct StubGit {
        has_changes: bool,
        files: Vec<String>,
    }

    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _repo_path: &str) -> anyhow::Result<bool> {
            Ok(self.has_changes)
        }

        fn changed_files(&self, _repo_path: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.files.clone())
        }
    }

    #[test]
    fn test_allows_non_edit_tools() {
        let git = StubGit {
            has_changes: true,
            files: vec!["a.rs".into(); 20],
        };
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_tool_name() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput::default();
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_edit_under_threshold() {
        let git = StubGit {
            has_changes: true,
            files: vec!["a.rs".into(), "b.rs".into()],
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_edit_over_threshold() {
        let files: Vec<String> = (0..15).map(|i| format!("file{i}.rs")).collect();
        let git = StubGit {
            has_changes: true,
            files,
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("15 uncommitted files"));
    }

    #[test]
    fn test_blocks_write_over_threshold() {
        let files: Vec<String> = (0..12).map(|i| format!("file{i}.rs")).collect();
        let git = StubGit {
            has_changes: true,
            files,
        };
        let input = HookInput {
            tool_name: Some("Write".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_allows_when_no_uncommitted_changes() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }
}
