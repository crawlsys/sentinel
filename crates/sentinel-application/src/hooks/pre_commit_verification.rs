//! Pre-Commit Verification Gate
//!
//! Blocks `git commit` and `git push` unless test/build evidence exists
//! in the session transcript.
//!
//! Two-layer verification (ported from Node.js pre-commit-verification.js):
//!   Layer 1 (regex): Did any tests/builds run? (fast, ~0ms)
//!   Layer 2 (AI):    Skipped for now — regex-only detection
//!
//! Override: session-scoped temp file (60s TTL, via hygiene_override module)

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;

/// Search all project directories for a session transcript JSONL.
/// Fallback when `input.transcript_path` is missing or points to
/// a worktree-scoped dir that doesn't contain the transcript file.
///
/// Returns the **largest** matching transcript, not the first — worktree-scoped
/// project dirs often have small/empty transcripts while the original project
/// dir has the real one with test evidence.
///
/// Also searches one level deeper (subdirectories within project dirs) to catch
/// cases where Claude Code nests transcripts under session-scoped subdirs.
fn find_transcript_by_session(fs: &dyn super::FileSystemPort, session_id: &str) -> Option<String> {
    let projects_dir = fs.home_dir()?.join(".claude").join("projects");
    if !fs.exists(&projects_dir) {
        return None;
    }
    let filename = format!("{session_id}.jsonl");
    let mut best: Option<(u64, String)> = None;
    let mut candidates_checked = 0u32;

    for path in fs.read_dir(&projects_dir).ok()?.into_iter() {
        if !fs.is_dir(&path) {
            continue;
        }

        // Check top-level: projects/{dir}/{session_id}.jsonl
        let candidate = path.join(&filename);
        if let Ok(meta) = fs.metadata(&candidate) {
            let size = meta.len();
            candidates_checked += 1;
            if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
                best = Some((size, candidate.to_string_lossy().to_string()));
            }
        }

        // Check one level deeper: projects/{dir}/{subdir}/{session_id}.jsonl
        // Claude Code sometimes nests transcripts in session-scoped subdirs
        if let Ok(subdirs) = fs.read_dir(&path) {
            for subdir in subdirs {
                if fs.is_dir(&subdir) {
                    let subpath = subdir.join(&filename);
                    if let Ok(meta) = fs.metadata(&subpath) {
                        let size = meta.len();
                        candidates_checked += 1;
                        if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
                            best = Some((size, subpath.to_string_lossy().to_string()));
                        }
                    }
                }
            }
        }
    }

    if let Some((size, ref path)) = best {
        eprintln!(
            "[sentinel] pre-commit-verify: transcript fallback found {} candidate(s), \
             best: {} ({} bytes)",
            candidates_checked, path, size
        );
    } else {
        eprintln!(
            "[sentinel] pre-commit-verify: transcript fallback found 0 candidates \
             for session '{}'",
            session_id
        );
    }

    best.map(|(_, path)| path)
}

/// Test command patterns that count as verification evidence
const TEST_COMMAND_PATTERNS: &[&str] = &[
    r"\bnpm\s+test\b",
    r"\bnpx\s+(vitest|jest|mocha|cypress)\b",
    r"\byarn\s+test\b",
    r"\bpnpm\s+test\b",
    r"\bcargo\s+test\b",
    r"\bcargo\s+build\b",
    r"\bpytest\b",
    r"\bgo\s+test\b",
    r"\bnpm\s+run\s+(test|build|lint|check|typecheck)\b",
    r"\btsc\b.*--noEmit",
    r"\bvitest\b",
    r"\bjest\b",
    r"\bmake\s+test\b",
    r"\bmake\s+build\b",
    r"\bnpm\s+run\s+build\b",
    r"\bdocker\s+build\b",
    r"\bdepot\s+build\b",
    r"\bdepot\s+list\s+builds\b",
];

/// Test output patterns that confirm tests ran
const TEST_OUTPUT_PATTERNS: &[&str] = &[
    r"\d+\s+pass(?:ing|ed)?",
    r"\d+\s+fail(?:ing|ed)?",
    r"exit code:?\s*0",
    r"tests?\s+suites?.*passed",
    r"PASS",
    r"BUILD SUCCESS",
    r"Successfully compiled",
    r"All \d+ tests? passed",
];

/// Path to the default override file (session-scoped via hygiene_override).
fn default_override_path(fs: &dyn super::FileSystemPort, session_id: &str) -> PathBuf {
    super::hygiene_override::verification_override_path(fs, session_id)
}

/// Check the transcript for test evidence (Layer 1: regex)
fn transcript_has_test_evidence(fs: &dyn super::FileSystemPort, transcript_path: &str) -> bool {
    let content = match fs.read_to_string(std::path::Path::new(transcript_path)) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Build regex patterns
    let cmd_patterns: Vec<Regex> = TEST_COMMAND_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    let output_patterns: Vec<Regex> = TEST_OUTPUT_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    // Check each line of the transcript
    for line in content.lines() {
        // Try to parse as JSON transcript entry
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            // Check assistant messages for Bash tool_use with test commands
            if entry.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(content_arr) = entry
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content_arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                            && block.get("name").and_then(|n| n.as_str()) == Some("Bash")
                        {
                            let cmd = block
                                .get("input")
                                .and_then(|i| i.get("command"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("");
                            for pattern in &cmd_patterns {
                                if pattern.is_match(cmd) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }

            // Check tool results for test output
            if entry.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                let text = entry
                    .get("content")
                    .map(|c| {
                        if let Some(s) = c.as_str() {
                            s.to_string()
                        } else if let Some(arr) = c.as_array() {
                            arr.iter()
                                .filter_map(|item| {
                                    item.as_str().map(String::from).or_else(|| {
                                        item.get("text").and_then(|t| t.as_str()).map(String::from)
                                    })
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default();

                for pattern in &output_patterns {
                    if pattern.is_match(&text) {
                        return true;
                    }
                }
            }
        }

        // **Attack #59 fix**: Removed raw line fallback. Only structured JSON entries
        // are trusted for test evidence. Raw text lines could be injected by an
        // attacker (e.g., `echo "Tests: 5 passed" >> transcript.jsonl`) to fake
        // passing test evidence without actually running tests.
    }

    false
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

    !BUILD_CONFIG_MARKERS
        .iter()
        .any(|f| dir.join(f).exists())
}

/// Trait for running `git diff` — injectable so tests can stub it.
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
    let range = if is_commit { "--cached" } else { "origin/HEAD..HEAD" };
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
    if super::hygiene_override::is_signed_override_active(fs, override_path, "verification", session_id)
    {
        return HookOutput::allow();
    }

    // Layer 1: Check transcript for test evidence.
    // Try input.transcript_path first, then fall back to searching by session ID.
    // In worktrees, Claude Code sends a transcript_path to a worktree-scoped
    // project dir that exists but is nearly empty (1 line). The real transcript
    // with test evidence is in the original project dir. So we check BOTH:
    // the provided path first, then the fallback if no evidence was found.
    eprintln!(
        "[sentinel] pre-commit-verify: session_id={}, transcript_path={:?}, cwd={:?}",
        session_id,
        input.transcript_path.as_deref().unwrap_or("(none)"),
        input.cwd.as_deref().unwrap_or("(none)")
    );

    if let Some(ref transcript_path) = input.transcript_path {
        let path = std::path::Path::new(transcript_path.as_str());
        let exists = fs.exists(path);
        let size = fs.metadata(path).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "[sentinel] pre-commit-verify: checking input.transcript_path: {} (exists={}, {} bytes)",
            transcript_path, exists, size
        );
        if transcript_has_test_evidence(fs, transcript_path) {
            eprintln!("[sentinel] pre-commit-verify: EVIDENCE FOUND in input.transcript_path");
            return HookOutput::allow();
        }
        eprintln!("[sentinel] pre-commit-verify: no evidence in input.transcript_path, trying fallback");
    } else {
        eprintln!("[sentinel] pre-commit-verify: no input.transcript_path, trying fallback");
    }

    // Fallback: search all project dirs for the largest transcript with this session ID
    if let Some(ref fallback_path) = find_transcript_by_session(fs, session_id) {
        let size = fs.metadata(std::path::Path::new(fallback_path)).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "[sentinel] pre-commit-verify: fallback transcript: {} ({} bytes)",
            fallback_path, size
        );
        if transcript_has_test_evidence(fs, fallback_path) {
            eprintln!("[sentinel] pre-commit-verify: EVIDENCE FOUND in fallback transcript");
            return HookOutput::allow();
        }
        eprintln!("[sentinel] pre-commit-verify: NO evidence in fallback transcript either");
    } else {
        eprintln!(
            "[sentinel] pre-commit-verify: fallback found NO transcript for session '{}'",
            session_id
        );
    }

    // No evidence found — BLOCK
    eprintln!("[sentinel] pre-commit-verify: BLOCKING — no test evidence found anywhere");
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

    HookOutput::block(super::block_context::append_block_context(message, input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::hygiene_override;
    use std::io::Write;

    #[test]
    fn test_allows_non_bash_tool() {
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({"file_path": "foo.rs"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_non_git_bash_command() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
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
        // Implements GitStatusPort directly — diff_names is the only method
        // is_docs_only_commit_with reaches; everything else returns defaults.
        struct StubCodeDiff;
        impl super::super::GitStatusPort for StubCodeDiff {
            fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> { Ok(vec![]) }
            fn current_branch(&self, _: &str) -> anyhow::Result<String> { Ok("main".into()) }
            fn is_worktree(&self, _: &str) -> bool { false }
            fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn repo_root(&self, _: &str) -> Option<String> { None }
            fn list_worktree_names(&self, _: &str) -> Vec<String> { Vec::new() }
            fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
            fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
            fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
                Some(vec!["src/main.rs".to_string()])
            }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
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
    fn test_allows_when_transcript_has_evidence() {
        // Create a temp transcript with test evidence
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmpfile,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"npm test"}}}}]}}}}"#
        )
        .unwrap();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'tested'"})),
            transcript_path: Some(tmpfile.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        // `transcript_has_test_evidence` reads via FileSystemPort; the default
        // StubFs returns `bail!()` for read_to_string, which would break the
        // evidence detection. Use a real-FS stub that delegates to std::fs.
        struct RealFsStub;
        impl super::super::FileSystemPort for RealFsStub {
            fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
            fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
            fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
            fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }
        let real_fs = RealFsStub;
        let stub_git = crate::hooks::test_support::StubGit;
        let stub_proc = crate::hooks::test_support::StubProcess;
        let stub_mcp = crate::hooks::test_support::StubMemoryMcp;
        let stub_env = crate::hooks::test_support::StubEnv::new();
        let ctx = super::super::HookContext {
            git: &stub_git,
            vector_store: None,
            fs: &real_fs,
            process: &stub_proc,
            llm: None,
            memory_mcp: &stub_mcp,
            env: &stub_env,
        };
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
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

        // For write/read roundtrip, use a real-FS wrapper
        struct RealTestFs;
        impl super::super::FileSystemPort for RealTestFs {
            fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
            fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
            fn write(&self, p: &std::path::Path, c: &[u8]) -> anyhow::Result<()> {
                if let Some(par) = p.parent() { std::fs::create_dir_all(par)?; }
                Ok(std::fs::write(p, c)?)
            }
            fn create_dir_all(&self, p: &std::path::Path) -> anyhow::Result<()> { Ok(std::fs::create_dir_all(p)?) }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
            fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
            fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }
        let real_fs = RealTestFs;

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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_transcript_output_patterns_detected() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmpfile,
            r#"{{"type":"tool_result","content":"5 passing (200ms)"}}"#
        )
        .unwrap();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            transcript_path: Some(tmpfile.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        // Use a real-FS stub so transcript_has_test_evidence can read the
        // tempfile we just wrote (default StubFs.read_to_string returns bail!).
        struct RealFsStub;
        impl super::super::FileSystemPort for RealFsStub {
            fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
            fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
            fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
            fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }
        let real_fs = RealFsStub;
        let stub_git = crate::hooks::test_support::StubGit;
        let stub_proc = crate::hooks::test_support::StubProcess;
        let stub_mcp = crate::hooks::test_support::StubMemoryMcp;
        let stub_env = crate::hooks::test_support::StubEnv::new();
        let ctx = super::super::HookContext {
            git: &stub_git,
            vector_store: None,
            fs: &real_fs,
            process: &stub_proc,
            llm: None,
            memory_mcp: &stub_mcp,
            env: &stub_env,
        };
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
    fn test_find_transcript_picks_largest_file() {
        // Simulates worktree bug: two project dirs have the same session JSONL,
        // but the worktree-scoped one is empty and the original has real content.
        let tmpdir = tempfile::tempdir().unwrap();
        let session_id = "test-find-largest";

        // Create two "project" dirs
        let worktree_dir = tmpdir.path().join("C--repo--worktree");
        let original_dir = tmpdir.path().join("C--repo");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::create_dir_all(&original_dir).unwrap();

        // Worktree transcript: empty
        let worktree_transcript = worktree_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&worktree_transcript, "").unwrap();

        // Original transcript: has test evidence
        let original_transcript = original_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &original_transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        )
        .unwrap();

        // Use a real-fs wrapper for read_to_string — `transcript_has_test_evidence`
        // takes a `FileSystemPort`, and we need actual file IO here.
        struct RealTestFs;
        impl super::super::FileSystemPort for RealTestFs {
            fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
            fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
            fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
            fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }
        let real_fs = RealTestFs;

        // The original (larger) transcript should have evidence
        assert!(transcript_has_test_evidence(
            &real_fs,
            &original_transcript.to_string_lossy()
        ));
        // The worktree (empty) one should not
        assert!(!transcript_has_test_evidence(
            &real_fs,
            &worktree_transcript.to_string_lossy()
        ));
    }

    #[test]
    fn test_docs_only_extensions() {
        // These should all be recognized as docs-only
        let docs_files = vec![
            "README.md", "CHANGELOG.md", "skills/linear/SKILL.md",
            "config.json", "config.yaml", "settings.toml",
            ".gitignore", ".editorconfig", "LICENSE",
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
            "main.rs", "index.ts", "app.tsx", "server.py", "handler.go",
            "style.css", "Cargo.toml",
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
        // Use an explicit stub so the test doesn't depend on the ambient git repo state.
        // Reports zero diffed files via GitStatusPort.diff_names.
        struct NoFiles;
        impl super::super::GitStatusPort for NoFiles {
            fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> { Ok(vec![]) }
            fn current_branch(&self, _: &str) -> anyhow::Result<String> { Ok("main".into()) }
            fn is_worktree(&self, _: &str) -> bool { false }
            fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn repo_root(&self, _: &str) -> Option<String> { None }
            fn list_worktree_names(&self, _: &str) -> Vec<String> { Vec::new() }
            fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
            fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
            fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { Some(vec![]) }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
        }
        // Non-commit/push commands short-circuit before touching git.
        assert!(!is_docs_only_commit_with("ls -la", &NoFiles, "."));
        // A push with no diff'd files returns false (can't determine → don't skip).
        assert!(!is_docs_only_commit_with("git push origin main", &NoFiles, "."));
    }
}
