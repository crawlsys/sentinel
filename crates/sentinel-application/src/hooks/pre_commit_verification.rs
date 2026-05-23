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
//! checks that file — keyed by the **same** session_id Claude Code passes in
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

// `BUILD_CONFIG_MARKERS`, `DOCS_ONLY_EXTENSIONS`, and the `is_docs_only_path`
// classifier live in `sentinel_domain::repo_kind` — pure rules with no IO.
// The hook keeps the IO half of `is_content_only_repo` (does file X exist?)
// and consults the marker list from the domain.
use sentinel_domain::repo_kind::{is_docs_only_path, BUILD_CONFIG_MARKERS};

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

/// Process a pre-commit verification hook event (PreToolUse).
/// Uses session-scoped signed override check.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let override_path = default_override_path(ctx.fs, session_id);
    process_with_override(input, &override_path, session_id, ctx.fs, ctx.git)
}

/// Internal: process with explicit override path + git port (for testability).
/// Tests call this directly with a stub `GitStatusPort` for diff determinism.
fn process_with_override(
    input: &HookInput,
    override_path: &std::path::Path,
    session_id: &str,
    fs: &dyn super::FileSystemPort,
    git: &dyn super::GitStatusPort,
) -> HookOutput {
    // Only act on Bash tool calls
    let tool = match &input.tool_name {
        Some(name) if name == "Bash" => name.as_str(),
        _ => return HookOutput::allow(),
    };
    let _ = tool;

    // Extract command from tool_input
    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    // Check if this is a git commit or git push
    let git_re = match Regex::new(r"\bgit\s+(commit|push)\b") {
        Ok(re) => re,
        Err(_) => return HookOutput::allow(),
    };

    let caps = match git_re.captures(command) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    let action = caps.get(1).map_or("commit", |m| m.as_str());
    let action_gerund = if action == "commit" {
        "Committing"
    } else {
        "Pushing"
    };

    // Skip verification for content-only repos (no package.json, Cargo.toml, etc.)
    // These repos have no test toolchain — requiring evidence is nonsensical.
    if is_content_only_repo(input.cwd.as_deref()) {
        eprintln!("[sentinel] pre-commit-verify: content-only repo (no build config), skipping");
        return HookOutput::allow();
    }

    // Skip verification for docs-only commits/pushes (markdown, config, YAML, etc.)
    // These files have no tests to run — requiring evidence is nonsensical.
    let cwd = input.cwd.as_deref().unwrap_or(".");
    if is_docs_only_commit_with(command, git, cwd) {
        return HookOutput::allow();
    }

    // Check signed override file (Attack #47: replaces mtime-only check)
    if super::hygiene_override::is_signed_override_active(
        fs,
        override_path,
        "verification",
        session_id,
    ) {
        return HookOutput::allow();
    }

    // Look up sentinel-recorded evidence for THIS session_id. The recorder
    // hook writes the file on PostToolUse with the same session_id we get
    // here, so the lookup is exact — no transcript parsing required.
    if session_has_recorded_evidence(fs, session_id) {
        eprintln!("[sentinel] pre-commit-verify: evidence file present for session '{session_id}'");
        return HookOutput::allow();
    }

    // No evidence found — BLOCK
    eprintln!(
        "[sentinel] pre-commit-verify: BLOCKING — no recorded test evidence for session '{session_id}'"
    );
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
    // Surface the block upstream so operators on remote surfaces
    // see their agent stuck waiting on verification. Fire-and-
    // forget; daemon outage is a silent no-op.
    super::upstream_block::signal_upstream("pre_commit_verification", &full);
    HookOutput::block(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::hygiene_override;
    use sentinel_domain::test_evidence::TestEvidenceEntry;
    use std::path::Path;

    /// Real-FS adapter scoped to a caller-supplied home directory.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl super::super::FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
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
        let output = process(&input, &ctx);
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
        let output = process(&input, &ctx);
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
            fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
                Ok(vec![])
            }
            fn current_branch(&self, _: &str) -> anyhow::Result<String> {
                Ok("main".into())
            }
            fn is_worktree(&self, _: &str) -> bool {
                false
            }
            fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
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
            fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
                Some(vec!["src/main.rs".to_string()])
            }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
        }
        let output = process_with_override(
            &input,
            &override_path,
            "test-sess",
            &crate::hooks::test_support::StubFs,
            &StubCodeDiff,
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
        let output = process_with_override(&input, &override_path, session_id, &fs, &git);
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
        let output = process_with_override(&input, &override_path, session_id, &fs, &git);
        assert_eq!(output.blocked, Some(true));
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
        let output = process(&input, &ctx);
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
            fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
                Ok(vec![])
            }
            fn current_branch(&self, _: &str) -> anyhow::Result<String> {
                Ok("main".into())
            }
            fn is_worktree(&self, _: &str) -> bool {
                false
            }
            fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
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
            fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
                Some(vec![])
            }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
                Vec::new()
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
