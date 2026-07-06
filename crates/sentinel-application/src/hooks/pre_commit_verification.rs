//! Pre-Commit Verification Gate
//!
//! Blocks `git commit` and `git push` unless test/build evidence exists
//! for the current session.
//!
//! ## Source of evidence
//!
//! Sentinel records its own evidence on `PostToolUse` (see
//! [`super::test_evidence_recorder`]). Every Bash invocation that matches a
//! test/build pattern is appended to
//! `~/.claude/sentinel/state/test-evidence/{session_id}.jsonl`. This hook
//! checks that file — keyed by the **same** `session_id` Claude Code passes in
//! — and allows the commit/push if it contains at least one entry.
//!
//! Why not parse Claude Code's transcript? The hook input `session_id`
//! (harness wrapper) does **not** match the on-disk transcript filename
//! (Claude Code's internal `G8.sessionId`). Searching by the wrong key never
//! finds the file, so the old transcript-based check falsely blocked every
//! commit. The new design uses sentinel's own session-keyed evidence so the
//! lookup is always exact.
//!
//! Override: session-scoped signed file (via [`super::hygiene_override`]).

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::test_evidence::evidence_path;
use std::path::PathBuf;

/// Path to the default override file (session-scoped via `hygiene_override`).
fn default_override_path(fs: &dyn super::FileSystemPort, session_id: &str) -> PathBuf {
    super::hygiene_override::verification_override_path(fs, session_id)
}

/// Does the sentinel-recorded evidence file exist (and contain at least
/// one entry) for this session?
///
/// Single source of truth for "have tests/builds run in this session?". The
/// recorder hook only writes when a command matches a test pattern, so the
/// mere existence of a non-empty file is sufficient evidence.
fn session_has_recorded_evidence(fs: &dyn super::FileSystemPort, session_id: &str) -> bool {
    let Some(home) = fs.home_dir() else {
        return false;
    };
    let path = evidence_path(&home, session_id);
    if !fs.exists(&path) {
        return false;
    }
    match fs.read_to_string(&path) {
        Ok(contents) => contents.lines().any(|line| !line.trim().is_empty()),
        Err(_) => false,
    }
}

/// Delegates to the canonical validator (`super::concrete_input_session_id`).
/// Previously the WEAKEST gate — rejected only empty ids, accepting
/// `unknown`/`default`/`..`/oversized. The canonical validator rejects them; on
/// rejection the override path is empty and verification stays enforced (fails
/// safe).
fn concrete_session_id(input: &HookInput) -> Option<&str> {
    super::concrete_input_session_id(input)
}

// `BUILD_CONFIG_MARKERS`, `DOCS_ONLY_EXTENSIONS`, and the `is_docs_only_path`
// classifier live in `sentinel_domain::repo_kind` — pure rules with no IO.
// The hook keeps the IO half of `is_content_only_repo` (does file X exist?)
// and queries the marker list from the domain.
use sentinel_domain::repo_kind::{is_docs_only_path, BUILD_CONFIG_MARKERS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreCommitAction {
    None,
    Commit,
    Push,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreCommitDecision {
    Allow,
    AllowContentOnlyRepo,
    AllowDocsOnly,
    AllowSignedOverride,
    AllowRecordedEvidence,
    Block,
}

#[derive(Debug, Clone)]
pub struct PreCommitVerificationEvaluation {
    pub tool: Option<String>,
    pub command: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub action: PreCommitAction,
    pub content_only_repo: bool,
    pub docs_only_change: bool,
    pub signed_override_active: bool,
    pub recorded_evidence_present: bool,
    pub session_id_present: bool,
    pub should_block: bool,
    pub decision: PreCommitDecision,
}

impl PreCommitVerificationEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.bash_tool && !matches!(self.action, PreCommitAction::None)
    }
}

/// Check if the current working directory is a content-only repo
/// (no build config files → no test toolchain → nothing to verify).
fn is_content_only_repo(cwd: Option<&str>) -> bool {
    let dir = match cwd {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => match std::env::current_dir() {
            Ok(d) => d,
            Err(_) => return false,
        },
    };

    !BUILD_CONFIG_MARKERS.iter().any(|f| dir.join(f).exists())
}

/// Check if a git commit/push only touches non-code files.
/// Returns true if ALL files have docs-only extensions.
fn is_docs_only_commit_with(command: &str, git: &dyn super::GitStatusPort, cwd: &str) -> bool {
    let is_commit = command.contains("commit");
    let is_push = command.contains("push");

    if !is_commit && !is_push {
        return false;
    }

    // Staged diff for commits, branch diff for pushes — same ranges as the
    // legacy `RealGitDiff` impl this replaced.
    let range = if is_commit {
        "--cached"
    } else {
        "origin/HEAD..HEAD"
    };
    let files = match git.diff_names(cwd, range) {
        Some(f) => f,
        None => return false, // Can't determine — don't skip
    };

    // No files — can't determine, don't skip verification
    if files.is_empty() {
        return false;
    }

    // Check every file against the domain docs-only classifier.
    files.iter().all(|f| is_docs_only_path(f))
}

/// Process a pre-commit verification hook event (`PreToolUse`).
/// Uses session-scoped signed override check.
pub fn process(
    input: &HookInput,
    ctx: &super::HookContext<'_>,
    state: &SessionState,
) -> HookOutput {
    let evaluation = evaluate(input, ctx, state);
    output_from_evaluation(input, &evaluation)
}

pub fn evaluate(
    input: &HookInput,
    ctx: &super::HookContext<'_>,
    state: &SessionState,
) -> PreCommitVerificationEvaluation {
    let session_id = concrete_session_id(input);
    let override_path = session_id
        .map(|session_id| default_override_path(ctx.fs, session_id))
        .unwrap_or_default();
    evaluate_with_override(input, &override_path, session_id, ctx.fs, ctx.git, state)
}

/// Internal: process with explicit override path + git port (for testability).
/// Tests call this directly with a stub `GitStatusPort` for diff determinism.
#[cfg(test)]
fn process_with_override(
    input: &HookInput,
    override_path: &std::path::Path,
    session_id: &str,
    fs: &dyn super::FileSystemPort,
    git: &dyn super::GitStatusPort,
    _state: &SessionState,
) -> HookOutput {
    let evaluation =
        evaluate_with_override(input, override_path, Some(session_id), fs, git, _state);
    output_from_evaluation(input, &evaluation)
}

pub fn evaluate_with_override(
    input: &HookInput,
    override_path: &std::path::Path,
    session_id: Option<&str>,
    fs: &dyn super::FileSystemPort,
    git: &dyn super::GitStatusPort,
    _state: &SessionState,
) -> PreCommitVerificationEvaluation {
    let tool = input.tool_name.clone();
    let bash_tool = matches!(input.tool_name.as_deref(), Some("Bash"));

    // Extract command from tool_input
    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let command_present = command.as_deref().is_some_and(|cmd| !cmd.is_empty());

    if !bash_tool {
        return base_evaluation(
            tool,
            command,
            bash_tool,
            command_present,
            PreCommitAction::None,
            session_id,
        );
    }
    let Some(command_text) = command.as_deref() else {
        return base_evaluation(
            tool,
            command,
            bash_tool,
            command_present,
            PreCommitAction::None,
            session_id,
        );
    };

    // Check if this is a git commit or git push
    let git_re = match Regex::new(r"\bgit\s+(commit|push)\b") {
        Ok(re) => re,
        Err(_) => {
            return base_evaluation(
                tool,
                command,
                bash_tool,
                command_present,
                PreCommitAction::None,
                session_id,
            );
        }
    };

    let caps = match git_re.captures(command_text) {
        Some(c) => c,
        None => {
            return base_evaluation(
                tool,
                command,
                bash_tool,
                command_present,
                PreCommitAction::None,
                session_id,
            );
        }
    };

    let action = if caps.get(1).is_some_and(|m| m.as_str() == "push") {
        PreCommitAction::Push
    } else {
        PreCommitAction::Commit
    };

    // Skip verification for content-only repos (no package.json, Cargo.toml, etc.)
    // These repos have no test toolchain — requiring evidence is nonsensical.
    let content_only_repo = is_content_only_repo(input.cwd.as_deref());

    // Skip verification for docs-only commits/pushes (markdown, config, YAML, etc.)
    // These files have no tests to run — requiring evidence is nonsensical.
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let docs_only_change = !content_only_repo && is_docs_only_commit_with(command_text, git, cwd);

    // Check signed override file (Attack #47: replaces mtime-only check)
    let signed_override_active = !content_only_repo
        && !docs_only_change
        && session_id.is_some_and(|session_id| {
            super::hygiene_override::is_signed_override_active(
                fs,
                override_path,
                "verification",
                session_id,
            )
        });

    // Look up sentinel-recorded evidence for THIS session_id. The recorder
    // hook writes the file on PostToolUse with the same session_id we get
    // here, so the lookup is exact — no transcript parsing required.
    let recorded_evidence_present = !content_only_repo
        && !docs_only_change
        && !signed_override_active
        && session_id.is_some_and(|session_id| session_has_recorded_evidence(fs, session_id));

    let decision = if content_only_repo {
        PreCommitDecision::AllowContentOnlyRepo
    } else if docs_only_change {
        PreCommitDecision::AllowDocsOnly
    } else if signed_override_active {
        PreCommitDecision::AllowSignedOverride
    } else if recorded_evidence_present {
        PreCommitDecision::AllowRecordedEvidence
    } else {
        PreCommitDecision::Block
    };

    PreCommitVerificationEvaluation {
        tool,
        command,
        bash_tool,
        command_present,
        action,
        content_only_repo,
        docs_only_change,
        signed_override_active,
        recorded_evidence_present,
        session_id_present: session_id.is_some(),
        should_block: matches!(decision, PreCommitDecision::Block),
        decision,
    }
}

fn base_evaluation(
    tool: Option<String>,
    command: Option<String>,
    bash_tool: bool,
    command_present: bool,
    action: PreCommitAction,
    session_id: Option<&str>,
) -> PreCommitVerificationEvaluation {
    PreCommitVerificationEvaluation {
        tool,
        command,
        bash_tool,
        command_present,
        action,
        content_only_repo: false,
        docs_only_change: false,
        signed_override_active: false,
        recorded_evidence_present: false,
        session_id_present: session_id.is_some(),
        should_block: false,
        decision: PreCommitDecision::Allow,
    }
}

pub fn output_from_evaluation(
    input: &HookInput,
    evaluation: &PreCommitVerificationEvaluation,
) -> HookOutput {
    match evaluation.decision {
        PreCommitDecision::Allow => return HookOutput::allow(),
        PreCommitDecision::AllowContentOnlyRepo => {
            eprintln!(
                "[sentinel] pre-commit-verify: content-only repo (no build config), skipping"
            );
            return HookOutput::allow();
        }
        PreCommitDecision::AllowDocsOnly | PreCommitDecision::AllowSignedOverride => {
            return HookOutput::allow();
        }
        PreCommitDecision::AllowRecordedEvidence => {
            if let Some(session_id) = concrete_session_id(input) {
                eprintln!(
                    "[sentinel] pre-commit-verify: evidence file present for session '{session_id}'"
                );
            } else {
                eprintln!(
                    "[sentinel] pre-commit-verify: evidence decision produced without a session id"
                );
            }
            return HookOutput::allow();
        }
        PreCommitDecision::Block => {}
    }

    let action_gerund = match evaluation.action {
        PreCommitAction::Push => "Pushing",
        _ => "Committing",
    };
    // No evidence found — BLOCK
    if let Some(session_id) = concrete_session_id(input) {
        eprintln!(
            "[sentinel] pre-commit-verify: BLOCKING — no recorded test evidence for session '{session_id}'"
        );
    } else {
        eprintln!(
            "[sentinel] pre-commit-verify: BLOCKING — no recorded test evidence; session id missing"
        );
    }
    let message = format!(
        "\
+============================================================+
|  BLOCKED: Run Tests Before {action_gerund:<34}|
+============================================================+
|  No test/build evidence found in this session.             |
|                                                            |
|  Run verification first:                                   |
|  -> npm test / vitest / jest / cargo test                  |
|  -> npm run build / tsc --noEmit                           |
|  -> npm run lint / npm run check                           |
|                                                            |
|  Or bypass:                                                |
|  -> Say \"override verification\" (5 min window)             |
+============================================================+
|  Evidence before claims. Always.                           |
+============================================================+"
    );

    let full = super::block_context::append_block_context(message, input);
    HookOutput::block(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::hygiene_override;
    use sentinel_domain::test_evidence::TestEvidenceEntry;
    use std::path::Path;

    /// Default session state for tests.
    fn nb() -> SessionState {
        SessionState::new("pre-commit-test")
    }

    /// Real-FS adapter scoped to a caller-supplied home directory.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl super::super::FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            _: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            f.write_all(c)?;
            Ok(())
        }
    }

    fn seed_evidence(home: &Path, session_id: &str, command: &str) {
        let path = evidence_path(home, session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let entry = TestEvidenceEntry {
            ts_ms: 1_700_000_000_000,
            session_id: session_id.into(),
            cwd: "/repo".into(),
            command: command.into(),
            success: true,
        };
        let line = serde_json::to_string(&entry).unwrap();
        std::fs::write(&path, format!("{line}\n")).unwrap();
    }

    #[test]
    fn test_allows_non_bash_tool() {
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({"file_path": "foo.rs"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx, &nb());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_non_git_bash_command() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx, &nb());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_git_commit_without_evidence() {
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'test'"})),
            ..Default::default()
        };
        let output = process_with_override(
            &input,
            &override_path,
            "test-sess",
            &crate::hooks::test_support::StubFs,
            &crate::hooks::test_support::StubGit,
            &nb(),
        );
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("BLOCKED"));
        assert!(output.reason.as_ref().unwrap().contains("Committing"));
    }

    #[test]
    fn test_blocks_git_push_without_evidence() {
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            ..Default::default()
        };
        // Stub git: pretend there are code files changed (not docs-only).
        struct StubCodeDiff;
        impl super::super::GitStatusPort for StubCodeDiff {
            fn has_uncommitted_changes(
                &self,
                _: &str,
            ) -> Result<bool, sentinel_domain::port_errors::GitError> {
                Ok(false)
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
                Some(vec!["src/main.rs".to_string()])
            }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn head_sha(&self, _: &str) -> Option<String> {
                None
            }
        }
        let output = process_with_override(
            &input,
            &override_path,
            "test-sess",
            &crate::hooks::test_support::StubFs,
            &StubCodeDiff,
            &nb(),
        );
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("Pushing"));
    }

    #[test]
    fn test_allows_when_recorded_evidence_present() {
        let tmpdir = tempfile::tempdir().unwrap();
        let session_id = "test-evidence-present";
        seed_evidence(tmpdir.path(), session_id, "cargo test");

        let fs = ScopedHomeFs {
            home: tmpdir.path().to_path_buf(),
        };
        let git = crate::hooks::test_support::StubGit;
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'tested'"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path, session_id, &fs, &git, &nb());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_when_evidence_file_empty() {
        let tmpdir = tempfile::tempdir().unwrap();
        let session_id = "test-evidence-empty";
        let path = evidence_path(tmpdir.path(), session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();

        let fs = ScopedHomeFs {
            home: tmpdir.path().to_path_buf(),
        };
        let git = crate::hooks::test_support::StubGit;
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'no tests'"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path, session_id, &fs, &git, &nb());
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_missing_session_id_ignores_unknown_evidence_and_override() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmpdir.path().join("Cargo.toml"),
            "[package]\nname = \"pre-commit-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        seed_evidence(tmpdir.path(), "unknown", "cargo test");

        let fs = ScopedHomeFs {
            home: tmpdir.path().to_path_buf(),
        };
        let override_path = tmpdir.path().join("unknown-override");
        hygiene_override::write_signed_override_for_test(
            &fs,
            &override_path,
            "verification",
            "unknown",
        );

        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'no session'"})),
            cwd: Some(tmpdir.path().to_string_lossy().to_string()),
            session_id: None,
            ..Default::default()
        };

        let evaluation = evaluate_with_override(
            &input,
            &override_path,
            None,
            &fs,
            &crate::hooks::test_support::StubGit,
            &nb(),
        );

        assert!(!evaluation.session_id_present);
        assert!(!evaluation.signed_override_active);
        assert!(!evaluation.recorded_evidence_present);
        assert_eq!(evaluation.decision, PreCommitDecision::Block);
        assert!(evaluation.should_block);

        let output = output_from_evaluation(&input, &evaluation);
        assert_eq!(output.blocked, Some(true));
        assert!(!output
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("unknown"));
    }

    #[test]
    fn test_allows_when_signed_override_active() {
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("test-override");
        let session_id = "test-sess-override";

        let test_fs = crate::hooks::test_support::StubFs;

        // No override file — should not be active
        assert!(!hygiene_override::is_signed_override_active(
            &test_fs,
            &override_path,
            "verification",
            session_id
        ));

        // For write/read roundtrip, use the real-FS wrapper
        let real_fs = ScopedHomeFs {
            home: tmpdir.path().to_path_buf(),
        };

        // Write a properly signed override file
        hygiene_override::write_signed_override_for_test(
            &real_fs,
            &override_path,
            "verification",
            session_id,
        );
        assert!(hygiene_override::is_signed_override_active(
            &real_fs,
            &override_path,
            "verification",
            session_id
        ));

        // Verify process respects it
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'override'"})),
            ..Default::default()
        };
        let output = process_with_override(
            &input,
            &override_path,
            session_id,
            &real_fs,
            &crate::hooks::test_support::StubGit,
            &nb(),
        );
        assert!(output.blocked.is_none());

        // Verify that a plain `touch` doesn't work
        let touch_path = tmpdir.path().join("touch-override");
        std::fs::write(&touch_path, "").unwrap();
        assert!(!hygiene_override::is_signed_override_active(
            &real_fs,
            &touch_path,
            "verification",
            session_id
        ));
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx, &nb());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_git_commit_amend() {
        // Use a non-existent override path to isolate from parallel tests
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit --amend"})),
            ..Default::default()
        };
        let output = process_with_override(
            &input,
            &override_path,
            "test-sess",
            &crate::hooks::test_support::StubFs,
            &crate::hooks::test_support::StubGit,
            &nb(),
        );
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_docs_only_extensions() {
        // These should all be recognized as docs-only
        let docs_files = vec![
            "README.md",
            "CHANGELOG.md",
            "skills/linear/SKILL.md",
            "config.json",
            "config.yaml",
            "settings.toml",
            ".gitignore",
            ".editorconfig",
            "LICENSE",
        ];
        for f in &docs_files {
            assert!(
                is_docs_only_path(f),
                "Expected '{f}' to be recognized as docs-only",
            );
        }

        // These should NOT be docs-only (`.toml` IS in the list, so it's
        // an exception we explicitly check).
        let code_files = vec![
            "main.rs",
            "index.ts",
            "app.tsx",
            "server.py",
            "handler.go",
            "style.css",
            "Cargo.toml",
        ];
        for f in &code_files {
            let is_docs = is_docs_only_path(f);
            if f.ends_with(".toml") {
                assert!(is_docs, ".toml should be docs-only");
            } else {
                assert!(!is_docs, "Expected '{f}' to NOT be docs-only");
            }
        }
    }

    #[test]
    fn test_is_docs_only_not_commit() {
        struct NoFiles;
        impl super::super::GitStatusPort for NoFiles {
            fn has_uncommitted_changes(
                &self,
                _: &str,
            ) -> Result<bool, sentinel_domain::port_errors::GitError> {
                Ok(false)
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
                Some(vec![])
            }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn head_sha(&self, _: &str) -> Option<String> {
                None
            }
        }
        // Non-commit/push commands short-circuit before touching git.
        assert!(!is_docs_only_commit_with("ls -la", &NoFiles, "."));
        // A push with no diff'd files returns false (can't determine → don't skip).
        assert!(!is_docs_only_commit_with(
            "git push origin main",
            &NoFiles,
            "."
        ));
    }
}
