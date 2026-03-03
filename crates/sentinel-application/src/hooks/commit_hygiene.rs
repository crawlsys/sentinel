//! Commit Hygiene
//!
//! Warns when Claude finishes responding with uncommitted changes.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::GitStatusPort;

/// Process a commit-hygiene hook event.
///
/// Accepts a `GitStatusPort` implementor so the application layer
/// stays decoupled from the infrastructure layer.
pub fn process(input: &HookInput, git: &dyn GitStatusPort) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    match git.has_uncommitted_changes(cwd) {
        Ok(true) => match git.changed_files(cwd) {
            Ok(files) if !files.is_empty() => {
                let context = format!(
                    "[Commit Hygiene] {} uncommitted file(s). \
                     Remember to commit your changes.",
                    files.len()
                );
                HookOutput::inject_context(HookEvent::Stop, context)
            }
            _ => HookOutput::allow(),
        },
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
    fn test_no_uncommitted_changes() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_uncommitted_changes_injects_context() {
        let git = StubGit {
            has_changes: true,
            files: vec!["src/main.rs".into(), "README.md".into()],
        };
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("2 uncommitted file(s)"));
        assert_eq!(ctx.hook_event_name, "Stop");
    }

    #[test]
    fn test_has_changes_but_empty_file_list() {
        let git = StubGit {
            has_changes: true,
            files: vec![],
        };
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_defaults_cwd_to_dot() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput::default();
        let output = process(&input, &git);
        assert!(output.hook_specific_output.is_none());
    }
}
