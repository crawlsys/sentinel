//! Pre-Push Browser Test Hook
//!
//! Blocks `git push` commands when a browser test hasn't been run in the
//! current session AND the push includes frontend file changes. Ensures
//! UI changes are browser-verified before code reaches the remote.
//!
//! "Browser test" here means either a local CDP test (`mcp__cdp`__*, edge-cdp
//! skill) for localhost OR a remote Browserbase test (`mcp__browserbase`__*)
//! for preview/staging URLs. Either one counts as proof that someone drove
//! the UI in a real browser since the last frontend change.
//!
//! This is the safety net behind Layer 2.5 (pre-push local browser test) in
//! the Linear review phase. If the skill-level gate was followed, the state
//! file will already exist and this hook allows the push instantly.
//!
//! Session state tracked via temp file: {tmpdir}/claude-browser-test-{session_id}.json
//! State format: {"passed": true, "sessionId": "...", "timestamp": "ISO8601"}
//!
//! Logic:
//! 1. Only fires on `git push` commands
//! 2. Matches cwd repo name against project configs with browser test settings
//! 3. If current repo has no matching browser-test-configured project → allow
//! 4. Checks if diff includes frontend files (.tsx, .jsx, .css, .scss, .styled)
//! 5. If no frontend files → allow (backend-only push)
//! 6. If frontend files + recent browser test → allow
//! 7. If frontend files + no browser test → block with instructions

use chrono::Utc;
use regex::Regex;
use sentinel_domain::constants;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;
use std::time::Duration;

/// Browser test validity window.
const TEST_VALIDITY: Duration = constants::BROWSER_TEST_VALIDITY;

/// Frontend file extensions that trigger browser test requirement
const FRONTEND_EXTENSIONS: &[&str] = &[".tsx", ".jsx", ".css", ".scss", ".styled"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrePushBrowserDecision {
    Allow,
    AllowNoBrowserConfig,
    AllowNoFrontendChanges,
    AllowRecentBrowserTest,
    Block,
}

#[derive(Debug, Clone)]
pub struct PrePushBrowserEvaluation {
    pub tool: Option<String>,
    pub command: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub git_push: bool,
    pub repo_browser_test_configured: bool,
    pub frontend_changes: bool,
    pub session_id_present: bool,
    pub recent_browser_test: bool,
    pub should_block: bool,
    pub decision: PrePushBrowserDecision,
}

impl PrePushBrowserEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.bash_tool && self.git_push
    }
}

/// Path to the browser test state file for a given session.
/// **Attack #61 fix**: Moved from world-writable `temp_dir()` to sentinel's
/// protected directory. Also sanitizes `session_id` to prevent path traversal.
fn state_file_path(fs: &dyn super::FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let id = concrete_session_id(session_id)?;
    if id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }

    Some(
        fs.claude_dir()
            .join("sentinel")
            .join("browser-test")
            .join(format!("{id}.json")),
    )
}

fn concrete_session_id(session_id: &str) -> Option<&str> {
    let id = session_id.trim();
    if id.is_empty() || id == "unknown" {
        None
    } else {
        Some(id)
    }
}

fn input_session_id(input: &HookInput) -> Option<&str> {
    input.session_id.as_deref().and_then(concrete_session_id)
}

/// Check if a passing browser test exists for this session within the validity window.
/// (Public wrapper for CLI access — caller injects the FS adapter.)
pub fn has_recent_browser_test_pub(fs: &dyn super::FileSystemPort, session_id: &str) -> bool {
    has_recent_browser_test(fs, session_id)
}

/// Check if a passing browser test exists for this session within the validity window.
fn has_recent_browser_test(fs: &dyn super::FileSystemPort, session_id: &str) -> bool {
    let Some(session_id) = concrete_session_id(session_id) else {
        return false;
    };
    let Some(path) = state_file_path(fs, session_id) else {
        return false;
    };
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let state: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Verify passed flag and session match
    let passed = state
        .get("passed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let state_session = state
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !passed || state_session != session_id {
        return false;
    }

    // Check timestamp is within validity window
    let timestamp = match state.get("timestamp").and_then(|v| v.as_str()) {
        Some(ts) => ts,
        None => return false,
    };

    match chrono::DateTime::parse_from_rfc3339(timestamp) {
        Ok(test_time) => {
            let elapsed = Utc::now().signed_duration_since(test_time);
            elapsed.num_seconds() >= 0 && elapsed.to_std().is_ok_and(|d| d < TEST_VALIDITY)
        }
        Err(_) => false,
    }
}

/// Extract the repo directory name from a cwd path.
/// e.g. "C:\Users\garys\Documents\GitHub\firefly-pro-crm" → "firefly-pro-crm"
/// Also handles worktree paths like "repo--branch-name" by stripping the "--" suffix.
fn repo_name_from_cwd(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    let name = path.file_name()?.to_string_lossy().to_string();
    // Strip worktree suffix (e.g. "firefly-pro-crm--fir-123-desc" → "firefly-pro-crm")
    let base = name.split("--").next().unwrap_or(&name);
    Some(base.to_lowercase())
}

/// Check if the current repo matches a project config that has browser test settings.
/// Scoped check: only returns true if the repo name matches the project's name or aliases
/// AND that project has `browser_test_email` configured.
///
/// Accepts an optional override path for testing; uses ~/.claude/skills/linear/projects/ by default.
fn repo_has_browser_test_config_in(
    fs: &dyn super::FileSystemPort,
    cwd: Option<&str>,
    projects_dir: Option<&std::path::Path>,
) -> bool {
    let repo = match cwd.and_then(repo_name_from_cwd) {
        Some(r) => r,
        None => return false, // No cwd → can't determine repo → allow
    };

    let default_dir = fs.home_dir().map(|h| {
        h.join(".claude")
            .join("skills")
            .join("linear")
            .join("projects")
    });

    let projects_dir = match projects_dir {
        Some(d) => d.to_path_buf(),
        None => match default_dir {
            Some(d) if fs.is_dir(&d) => d,
            _ => return false,
        },
    };

    if !fs.is_dir(&projects_dir) {
        return false;
    }

    let entries = match fs.read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    for path in entries {
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('_'))
        {
            continue;
        }

        if let Ok(content) = fs.read_to_string(&path) {
            // Must have browser_test_email to be a browser-test-configured project.
            if !content.contains("browser_test_email") {
                continue;
            }

            // Check if repo name matches project name or aliases
            let content_lower = content.to_lowercase();
            if repo_matches_project(&repo, &content_lower) {
                return true;
            }
        }
    }

    false
}

/// Check if a repo name matches a project config's name or aliases.
/// Matches against:
/// - `name:` frontmatter field (e.g. "name: firefly-pro")
/// - `aliases:` frontmatter array (e.g. aliases: [`firefly`, `crm`, `fpcrm`])
/// - The filename stem of the project file
///
/// Repo names are matched with normalization: "firefly-pro-crm" matches alias "fpcrm",
/// name "firefly-pro", and common variants like "firefly" by checking if the repo
/// name contains or is contained by any alias.
fn repo_matches_project(repo: &str, content_lower: &str) -> bool {
    // Extract name field: `name: firefly-pro`
    for line in content_lower.lines() {
        let trimmed = line.trim();
        if let Some(name_val) = trimmed.strip_prefix("name:") {
            let name = name_val.trim().trim_matches('"');
            if repo.contains(name) || name.contains(repo) {
                return true;
            }
        }

        // Extract aliases array: `aliases: ["firefly", "crm", "fpcrm"]`
        if let Some(aliases_val) = trimmed.strip_prefix("aliases:") {
            let aliases_str = aliases_val.trim();
            // Parse simple array format: ["a", "b", "c"]
            let cleaned = aliases_str.trim_start_matches('[').trim_end_matches(']');
            for alias in cleaned.split(',') {
                let alias = alias.trim().trim_matches('"').trim_matches('\'');
                if alias.is_empty() {
                    continue;
                }
                if repo.contains(alias) || alias.contains(repo) {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if the current repo has browser test config (default projects path).
fn repo_has_browser_test_config(fs: &dyn super::FileSystemPort, cwd: Option<&str>) -> bool {
    repo_has_browser_test_config_in(fs, cwd, None)
}

/// Check if the git diff (staged or branch) includes frontend file changes.
/// Uses the working directory from the hook input.
///
/// Previous implementations used `origin/main..HEAD`, which includes every
/// commit merged into main since the last `git fetch` — so a fresh
/// backend-only branch got blamed for `.tsx`/`.css` files in earlier PRs.
/// The merge-base approach is fetch-agnostic and always scoped to *this*
/// branch's own changes.
///
/// We pick the candidate base whose merge-base is **most recent** (fewest
/// commits between base and HEAD), not simply the first that resolves. Reason:
/// after `git rebase origin/main && git push --force-with-lease`, `@{upstream}`
/// still points at the pre-rebase remote SHA; its merge-base with HEAD is far
/// older than `origin/main`'s. Preferring the nearer base correctly scopes the
/// diff to only the commits that are truly "new" on this branch vs. main.
fn diff_has_frontend_files(git: &dyn super::GitStatusPort, cwd: Option<&str>) -> bool {
    let dir = cwd.unwrap_or(".");

    // Candidate base refs. We evaluate ALL of them and pick the one whose
    // merge-base is closest to HEAD (shortest commit distance).
    let candidates = [
        "@{upstream}",
        "main",
        "origin/main",
        "master",
        "origin/master",
    ];

    let best_base = candidates
        .iter()
        .filter_map(|r| {
            let base = git.merge_base(dir, r)?;
            let distance = git.rev_list_count(dir, &base)?;
            Some((distance, base))
        })
        // Smallest distance = most recent common ancestor = tightest scope.
        .min_by_key(|(d, _)| *d)
        .map(|(_, base)| base);

    let Some(base) = best_base else {
        // No base resolved — allow push.
        return false;
    };

    let files = match git.diff_names(dir, &format!("{base}..HEAD")) {
        Some(f) => f,
        None => return false,
    };

    files
        .iter()
        .any(|line| FRONTEND_EXTENSIONS.iter().any(|ext| line.ends_with(ext)))
}

/// Write the browser test state file after a successful browser test session.
/// Called from the `PostToolUse` handler when:
///   - `mcp__browserbase__release_session` succeeds (Browserbase remote test), OR
///   - `mcp__cdp__close_instance` succeeds (CDP local test).
pub fn record_browser_test_passed(fs: &dyn super::FileSystemPort, session_id: &str) {
    let Some(session_id) = concrete_session_id(session_id) else {
        tracing::warn!("Refusing to write browser test state without a concrete session id");
        return;
    };
    let Some(path) = state_file_path(fs, session_id) else {
        tracing::warn!("Refusing to write browser test state for malformed session id");
        return;
    };
    // Ensure parent directory exists (Attack #61: now in ~/.claude/sentinel/browser-test/)
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let state = serde_json::json!({
        "passed": true,
        "sessionId": session_id,
        "timestamp": Utc::now().to_rfc3339()
    });
    if let Err(e) = fs.write(
        &path,
        serde_json::to_string(&state).unwrap_or_default().as_bytes(),
    ) {
        tracing::warn!("Failed to write browser test state file: {e}");
    } else {
        tracing::debug!("Browser test state recorded at {}", path.display());
    }
}

/// `PostToolUse` handler — detect successful browser tests and record test state.
/// Triggers on:
/// 1. `mcp__browserbase__release_session` — Browserbase remote test completed
/// 2. `mcp__cdp__close_instance`         — CDP local test completed
/// 3. Bash tool result containing `BROWSER_TEST_PASS` — CDP/Puppeteer/Playwright
///    script test completed.
///
/// Should be called from the `PostToolUse` event dispatch in `hook_cmd.rs`.
pub fn process_post_tool(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let tool = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return HookOutput::allow(),
    };

    let Some(session_id) = input_session_id(input) else {
        return HookOutput::allow();
    };

    // Path 1-2: any current MCP browser-test completion signal.
    if matches!(
        tool,
        "mcp__browserbase__release_session" | "mcp__cdp__close_instance"
    ) {
        record_browser_test_passed(ctx.fs, session_id);
        return HookOutput::allow();
    }

    // Path 3: Bash tool with BROWSER_TEST_PASS marker in output. Supports CDP,
    // Puppeteer, Playwright, or any browser test that prints the marker on success.
    if tool == "Bash" {
        let has_marker = input
            .tool_result
            .as_ref()
            .and_then(|r| r.as_str())
            .is_some_and(|s| s.contains("BROWSER_TEST_PASS"));
        if has_marker {
            record_browser_test_passed(ctx.fs, session_id);
        }
    }

    HookOutput::allow()
}

/// Process a pre-push browser test hook event (`PreToolUse`).
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let evaluation = evaluate(input, ctx);
    output_from_evaluation(input, &evaluation)
}

pub fn evaluate(input: &HookInput, ctx: &super::HookContext<'_>) -> PrePushBrowserEvaluation {
    let tool = input.tool_name.clone();
    let bash_tool = matches!(input.tool_name.as_deref(), Some("Bash"));
    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let command_present = command.as_deref().is_some_and(|cmd| !cmd.is_empty());
    let session_id = input_session_id(input);
    let session_id_present = session_id.is_some();

    if !bash_tool {
        return base_evaluation(
            tool,
            command,
            bash_tool,
            command_present,
            session_id_present,
        );
    }
    let Some(command_text) = command.as_deref() else {
        return base_evaluation(
            tool,
            command,
            bash_tool,
            command_present,
            session_id_present,
        );
    };

    // Check if this is a git push
    let push_re = match Regex::new(r"\bgit\s+push\b") {
        Ok(re) => re,
        Err(_) => {
            return base_evaluation(
                tool,
                command,
                bash_tool,
                command_present,
                session_id_present,
            );
        }
    };

    if !push_re.is_match(command_text) {
        return base_evaluation(
            tool,
            command,
            bash_tool,
            command_present,
            session_id_present,
        );
    }

    // Check if THIS repo's project has browser test config (not all projects globally)
    let cwd = input.cwd.as_deref();
    let repo_browser_test_configured = repo_has_browser_test_config(ctx.fs, cwd);
    if !repo_browser_test_configured {
        return PrePushBrowserEvaluation {
            tool,
            command,
            bash_tool,
            command_present,
            git_push: true,
            repo_browser_test_configured,
            frontend_changes: false,
            session_id_present,
            recent_browser_test: false,
            should_block: false,
            decision: PrePushBrowserDecision::AllowNoBrowserConfig,
        };
    }

    // Check if the diff includes frontend files
    let frontend_changes = diff_has_frontend_files(ctx.git, cwd);
    if !frontend_changes {
        // Backend-only change — no browser test needed
        return PrePushBrowserEvaluation {
            tool,
            command,
            bash_tool,
            command_present,
            git_push: true,
            repo_browser_test_configured,
            frontend_changes,
            session_id_present,
            recent_browser_test: false,
            should_block: false,
            decision: PrePushBrowserDecision::AllowNoFrontendChanges,
        };
    }

    // Get session ID
    // Check if a browser test passed recently
    let recent_browser_test =
        session_id.is_some_and(|session_id| has_recent_browser_test(ctx.fs, session_id));
    if recent_browser_test {
        return PrePushBrowserEvaluation {
            tool,
            command,
            bash_tool,
            command_present,
            git_push: true,
            repo_browser_test_configured,
            frontend_changes,
            session_id_present,
            recent_browser_test,
            should_block: false,
            decision: PrePushBrowserDecision::AllowRecentBrowserTest,
        };
    }

    PrePushBrowserEvaluation {
        tool,
        command,
        bash_tool,
        command_present,
        git_push: true,
        repo_browser_test_configured,
        frontend_changes,
        session_id_present,
        recent_browser_test,
        should_block: true,
        decision: PrePushBrowserDecision::Block,
    }
}

fn base_evaluation(
    tool: Option<String>,
    command: Option<String>,
    bash_tool: bool,
    command_present: bool,
    session_id_present: bool,
) -> PrePushBrowserEvaluation {
    PrePushBrowserEvaluation {
        tool,
        command,
        bash_tool,
        command_present,
        git_push: false,
        repo_browser_test_configured: false,
        frontend_changes: false,
        session_id_present,
        recent_browser_test: false,
        should_block: false,
        decision: PrePushBrowserDecision::Allow,
    }
}

pub fn output_from_evaluation(
    input: &HookInput,
    evaluation: &PrePushBrowserEvaluation,
) -> HookOutput {
    if !matches!(evaluation.decision, PrePushBrowserDecision::Block) {
        return HookOutput::allow();
    }

    // Block — frontend files changed but no browser test run
    let message = "\
+=============================================================+
|  BLOCKED: Browser Test Required — Frontend Changes Detected |
+=============================================================+
|  Your push includes frontend file changes (.tsx/.jsx/.css)  |
|  but no browser test has been run this session.             |
|                                                             |
|  Run Layer 2.5 (Pre-Push Local CDP Test) first:             |
|  1. Start local dev server                                  |
|  2. mcp__cdp__launch(name: \"local\", url: \"http://...\")  |
|  3. Navigate → login → screenshot → verify                  |
|  4. Check console errors                                    |
|  5. mcp__cdp__close_instance(name: \"local\")               |
|                                                             |
|  For remote URLs (preview, staging) use mcp__browserbase__* |
|  Or push manually from your terminal:                       |
|  -> git push origin <branch>                                |
+=============================================================+"
        .to_string();

    HookOutput::block(super::block_context::append_block_context(message, input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    /// Real-disk FileSystemPort stub for tests that use tempfile-backed
    /// directories. Only the methods actually exercised here are wired up;
    /// the rest fall through to default behaviour.
    struct RealFsTest;
    impl super::super::FileSystemPort for RealFsTest {
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            dirs::home_dir()
        }
        fn read_to_string(
            &self,
            p: &std::path::Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &std::path::Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(
            &self,
            p: &std::path::Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &std::path::Path,
        ) -> Result<Vec<std::path::PathBuf>, sentinel_domain::port_errors::FileSystemError>
        {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &std::path::Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &std::path::Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &std::path::Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            p: &std::path::Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            use std::io::Write;
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            f.write_all(c)?;
            Ok(())
        }
    }

    /// Build a `HookContext` whose `fs` is the real-disk adapter so
    /// tests of `record_browser_test_passed` actually persist a file.
    fn real_fs_ctx() -> super::super::HookContext<'static> {
        use crate::hooks::test_support::*;
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let fs: &'static RealFsTest = Box::leak(Box::new(RealFsTest));
        let process: &'static StubProcess = Box::leak(Box::new(StubProcess));
        let memory_mcp: &'static StubMemoryMcp = Box::leak(Box::new(StubMemoryMcp));
        let env: &'static StubEnv = Box::leak(Box::new(StubEnv::new()));
        super::super::HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        }
    }

    #[test]
    fn test_allows_non_bash_tool() {
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_non_push_command() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'test'"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_push_when_no_browser_test_config() {
        // Use an empty temp dir — no project config files with browser-test settings
        let tmpdir = tempfile::tempdir().unwrap();
        let result = repo_has_browser_test_config_in(
            &RealFsTest,
            Some("/fake/path/some-repo"),
            Some(tmpdir.path()),
        );
        assert!(
            !result,
            "Empty directory should have no browser-test config"
        );
    }

    #[test]
    fn test_detects_browser_test_config_for_matching_repo() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("firefly.md");
        std::fs::write(
            &project_file,
            "name: firefly-pro\naliases: [\"firefly\", \"crm\", \"fpcrm\"]\nbrowser_test_email: test@example.com",
        )
        .unwrap();
        // Repo name "firefly-pro-crm" contains alias "crm" → match
        let result = repo_has_browser_test_config_in(
            &RealFsTest,
            Some("/fake/path/firefly-pro-crm"),
            Some(tmpdir.path()),
        );
        assert!(
            result,
            "Should match repo 'firefly-pro-crm' against alias 'crm'"
        );
    }

    #[test]
    fn test_ignores_browser_test_config_for_unrelated_repo() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("firefly.md");
        std::fs::write(
            &project_file,
            "name: firefly-pro\naliases: [\"firefly\", \"crm\", \"fpcrm\"]\nbrowser_test_email: test@example.com",
        )
        .unwrap();
        // Repo name "sentinel" doesn't match any alias → no block
        let result = repo_has_browser_test_config_in(
            &RealFsTest,
            Some("/fake/path/sentinel"),
            Some(tmpdir.path()),
        );
        assert!(
            !result,
            "Should NOT match repo 'sentinel' against firefly aliases"
        );
    }

    #[test]
    fn test_ignores_project_without_browser_test_email() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("myproject.md");
        // Has staging_url but NOT browser_test_email → should not trigger browser gate
        std::fs::write(
            &project_file,
            "name: myproject\naliases: [\"myapp\"]\nstaging_url: https://staging.example.com",
        )
        .unwrap();
        let result = repo_has_browser_test_config_in(
            &RealFsTest,
            Some("/fake/path/myproject"),
            Some(tmpdir.path()),
        );
        assert!(
            !result,
            "Should NOT match project without browser_test_email"
        );
    }

    #[test]
    fn test_ignores_retired_steel_config_for_matching_repo() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("firefly.md");
        std::fs::write(
            &project_file,
            "name: firefly-pro\naliases: [\"firefly\", \"crm\", \"fpcrm\"]\nsteel_test_email: test@example.com",
        )
        .unwrap();

        let result = repo_has_browser_test_config_in(
            &RealFsTest,
            Some("/fake/path/firefly-pro-crm"),
            Some(tmpdir.path()),
        );
        assert!(
            !result,
            "retired steel_test_email must not configure browser-test gating"
        );
    }

    #[test]
    fn test_worktree_path_strips_branch_suffix() {
        assert_eq!(
            repo_name_from_cwd("/path/to/firefly-pro-crm--fir-123-desc"),
            Some("firefly-pro-crm".to_string())
        );
        assert_eq!(
            repo_name_from_cwd("/path/to/sentinel"),
            Some("sentinel".to_string())
        );
    }

    #[test]
    fn test_browser_test_state_requires_concrete_session_id() {
        assert!(state_file_path(&RealFsTest, "unknown").is_none());
        assert!(state_file_path(&RealFsTest, " ../escape ").is_none());
        assert!(!has_recent_browser_test(&RealFsTest, "unknown"));

        let unknown_path = <RealFsTest as super::super::FileSystemPort>::claude_dir(&RealFsTest)
            .join("sentinel")
            .join("browser-test")
            .join("unknown.json");
        let _ = std::fs::remove_file(&unknown_path);
        record_browser_test_passed(&RealFsTest, "unknown");
        assert!(
            !unknown_path.exists(),
            "missing session identity must not write unknown.json"
        );
    }

    #[test]
    fn test_evaluate_treats_unknown_session_as_missing() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            session_id: Some(" unknown ".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let evaluation = evaluate(&input, &ctx);
        assert!(evaluation.git_push);
        assert!(!evaluation.session_id_present);
        assert!(!evaluation.recent_browser_test);
    }

    #[test]
    fn test_allows_push_with_recent_browser_test() {
        let session_id = "test-steel-recent";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");

        // Write a valid recent state file
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let state = serde_json::json!({
            "passed": true,
            "sessionId": session_id,
            "timestamp": Utc::now().to_rfc3339()
        });
        let mut file = std::fs::File::create(&state_path).unwrap();
        write!(file, "{}", serde_json::to_string(&state).unwrap()).unwrap();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());

        // Cleanup
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_expired_browser_test_not_valid() {
        let session_id = "test-steel-expired";
        let result = has_recent_browser_test(&RealFsTest, session_id);
        assert!(!result);
    }

    #[test]
    fn test_mismatched_session_not_valid() {
        let session_id = "test-steel-mismatch";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let state = serde_json::json!({
            "passed": true,
            "sessionId": "different-session",
            "timestamp": Utc::now().to_rfc3339()
        });
        std::fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();

        let result = has_recent_browser_test(&RealFsTest, session_id);
        assert!(!result);

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_frontend_extensions_list() {
        // Verify our extension list covers the expected files
        assert!(FRONTEND_EXTENSIONS.contains(&".tsx"));
        assert!(FRONTEND_EXTENSIONS.contains(&".jsx"));
        assert!(FRONTEND_EXTENSIONS.contains(&".css"));
        assert!(FRONTEND_EXTENSIONS.contains(&".scss"));
        assert!(FRONTEND_EXTENSIONS.contains(&".styled"));
    }

    #[test]
    fn test_diff_has_frontend_files_non_git_dir() {
        // Non-git directory should return false (allow push)
        let tmpdir = tempfile::tempdir().unwrap();
        let result = diff_has_frontend_files(&RealTestGit, Some(tmpdir.path().to_str().unwrap()));
        assert!(!result, "Non-git dir should return false (allow push)");
    }

    /// Test-only `GitStatusPort` impl that shells out to real git. Tests
    /// drive `diff_has_frontend_files` against actual repos created in
    /// `tempfile::tempdir()`, so they need real git resolution. Unrelated
    /// methods return safe defaults — the tests exercise only the diff path.
    struct RealTestGit;
    impl super::super::GitStatusPort for RealTestGit {
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
        fn merge_base(&self, repo_path: &str, base_ref: &str) -> Option<String> {
            let out = std::process::Command::new("git")
                .args(["merge-base", "HEAD", base_ref])
                .current_dir(repo_path)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.is_empty() {
                None
            } else {
                Some(sha)
            }
        }
        fn rev_list_count(&self, repo_path: &str, from: &str) -> Option<u32> {
            let out = std::process::Command::new("git")
                .args(["rev-list", "--count", &format!("{from}..HEAD")])
                .current_dir(repo_path)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        }
        fn rev_list_count_range(&self, repo_path: &str, range: &str) -> Option<u32> {
            let out = std::process::Command::new("git")
                .args(["rev-list", "--count", range])
                .current_dir(repo_path)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        }
        fn diff_names(&self, repo_path: &str, range: &str) -> Option<Vec<String>> {
            let out = std::process::Command::new("git")
                .args(["diff", "--name-only", range])
                .current_dir(repo_path)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            Some(
                stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect(),
            )
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn head_sha(&self, repo_path: &str) -> Option<String> {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo_path)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!sha.is_empty()).then_some(sha)
        }
    }

    /// Helper: run `git` in a directory and assert success.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command failed to spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn test_diff_scoped_to_branch_ignores_prior_frontend_merges() {
        // Regression for Apr 2026: a backend-only branch off fresh `main`
        // was blocked because `origin/main..HEAD` included a prior frontend
        // PR that had merged after the last fetch. Merge-base against local
        // `main` should scope the diff to just this branch.
        let tmpdir = tempfile::tempdir().unwrap();
        let repo = tmpdir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@t.com"]);
        git(repo, &["config", "user.name", "Test"]);

        // Initial commit on main
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-q", "-m", "init"]);

        // Merge a frontend PR into main AFTER the feature branch will be cut.
        // First, cut the feature branch from current main.
        git(repo, &["branch", "feature/backend-only"]);

        // Now simulate an older frontend PR landing on main.
        std::fs::write(repo.join("App.tsx"), "x").unwrap();
        git(repo, &["add", "App.tsx"]);
        git(repo, &["commit", "-q", "-m", "ui: old frontend PR"]);

        // Switch to the backend-only feature branch and add a server-only commit.
        git(repo, &["checkout", "-q", "feature/backend-only"]);
        std::fs::write(repo.join("db.ts"), "export {}").unwrap();
        git(repo, &["add", "db.ts"]);
        git(repo, &["commit", "-q", "-m", "fix: backend-only"]);

        // With the old `origin/main..HEAD` logic this would see App.tsx and
        // return true. With merge-base, it should see only db.ts.
        let result = diff_has_frontend_files(&RealTestGit, Some(repo.to_str().unwrap()));
        assert!(
            !result,
            "Backend-only branch should not be flagged as frontend change"
        );
    }

    #[test]
    fn test_diff_scoped_ignores_stale_upstream_after_rebase() {
        // Regression for Apr 24 2026: a backend-only branch, after `git rebase
        // origin/main` + `git push --force-with-lease`, is blocked because
        // `@{upstream}` still points at the pre-rebase SHA. The merge-base
        // against that stale upstream is far older than the merge-base against
        // `origin/main`, so picking the first-resolving candidate incorrectly
        // scopes the diff to include upstream frontend commits.
        //
        // Fix: pick the candidate whose merge-base is *closest* to HEAD.
        let tmpdir = tempfile::tempdir().unwrap();
        let repo = tmpdir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@t.com"]);
        git(repo, &["config", "user.name", "Test"]);

        // Initial commit on main
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-q", "-m", "init"]);

        // Cut feature branch here — this is the "old" base of the branch.
        git(repo, &["checkout", "-q", "-b", "feature/backend"]);
        std::fs::write(repo.join("first-backend.ts"), "export {}").unwrap();
        git(repo, &["add", "first-backend.ts"]);
        git(repo, &["commit", "-q", "-m", "feat: first backend commit"]);

        // Set up a fake origin that mirrors this feature branch as its upstream.
        // We simulate "upstream is the pre-rebase SHA" by creating an
        // `origin/feature/backend` ref pointing at the current HEAD.
        let pre_rebase_head = {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // Create a fake remote-tracking ref and point branch.feature/backend's
        // upstream at it. We skip the `git branch --set-upstream-to` path
        // because it requires a real refs/heads/... on the remote; the two
        // `git config` lines below are exactly what it would write.
        git(
            repo,
            &[
                "update-ref",
                "refs/remotes/origin/feature/backend",
                &pre_rebase_head,
            ],
        );
        git(repo, &["config", "branch.feature/backend.remote", "origin"]);
        git(
            repo,
            &[
                "config",
                "branch.feature/backend.merge",
                "refs/heads/feature/backend",
            ],
        );

        // Now main advances with a frontend PR (simulating another PR merging
        // while our branch was in flight).
        git(repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("App.tsx"), "x").unwrap();
        git(repo, &["add", "App.tsx"]);
        git(
            repo,
            &["commit", "-q", "-m", "ui: frontend PR lands on main"],
        );
        // Mirror it into origin/main so the hook's candidate resolves.
        let new_main = {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(repo, &["update-ref", "refs/remotes/origin/main", &new_main]);

        // Rebase feature branch onto the new main — our commit replays on top.
        git(repo, &["checkout", "-q", "feature/backend"]);
        git(repo, &["rebase", "-q", "main"]);
        // Add a second backend commit, the CI fix.
        std::fs::write(repo.join("ci-fix.ts"), "export {}").unwrap();
        git(repo, &["add", "ci-fix.ts"]);
        git(repo, &["commit", "-q", "-m", "fix: ci backend"]);

        // Now simulate the push. @{upstream} (origin/feature/backend) is still
        // at pre_rebase_head. merge-base(HEAD, @{upstream}) is pre-rebase,
        // which is BEFORE main advanced with App.tsx. merge-base(HEAD, origin/main)
        // is the new_main SHA, which is AFTER App.tsx landed.
        //
        // The old "first resolving" logic picks @{upstream} → sees App.tsx →
        // falsely blocks. The fixed "most recent merge-base" logic picks
        // origin/main → sees only backend files → allows.
        let result = diff_has_frontend_files(&RealTestGit, Some(repo.to_str().unwrap()));
        assert!(
            !result,
            "Rebased backend-only branch must not be flagged because \
             @{{upstream}} still points at pre-rebase SHA",
        );
    }

    #[test]
    fn test_diff_detects_frontend_on_own_branch() {
        let tmpdir = tempfile::tempdir().unwrap();
        let repo = tmpdir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@t.com"]);
        git(repo, &["config", "user.name", "Test"]);

        std::fs::write(repo.join("README.md"), "hi").unwrap();
        git(repo, &["add", "README.md"]);
        git(repo, &["commit", "-q", "-m", "init"]);

        git(repo, &["checkout", "-q", "-b", "feature/ui"]);
        std::fs::write(repo.join("Component.tsx"), "x").unwrap();
        git(repo, &["add", "Component.tsx"]);
        git(repo, &["commit", "-q", "-m", "ui: new component"]);

        assert!(
            diff_has_frontend_files(&RealTestGit, Some(repo.to_str().unwrap())),
            "Frontend change on own branch should be detected"
        );
    }

    #[test]
    fn test_record_browser_test_passed_writes_state_file() {
        let session_id = "test-record-steel";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");

        // Ensure clean state
        let _ = std::fs::remove_file(&state_path);

        record_browser_test_passed(&RealFsTest, session_id);

        assert!(state_path.exists(), "State file should be created");
        let content = std::fs::read_to_string(&state_path).unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["passed"], true);
        assert_eq!(state["sessionId"], session_id);
        assert!(state["timestamp"].is_string());

        // Verify it's recognized as a recent test
        assert!(has_recent_browser_test(&RealFsTest, session_id));

        // Cleanup
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_process_post_tool_records_on_release() {
        let session_id = "test-post-tool-release";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");
        let _ = std::fs::remove_file(&state_path);

        // Claude Code does NOT populate tool_result for MCP tools —
        // PostToolUse firing is sufficient proof the call succeeded
        let input = HookInput {
            tool_name: Some("mcp__browserbase__release_session".to_string()),
            tool_result: None,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let ctx = real_fs_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(
            has_recent_browser_test(&RealFsTest, session_id),
            "State file should be written after release"
        );

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_process_post_tool_ignores_retired_steel_release() {
        let session_id = "test-post-tool-steel-release";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("mcp__steel__release_session".to_string()),
            tool_result: None,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let ctx = real_fs_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(
            !state_path.exists(),
            "retired Steel release event must not satisfy browser-test evidence"
        );
    }

    #[test]
    fn test_process_post_tool_ignores_bash_without_marker() {
        let session_id = "test-post-tool-no-marker";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_result: Some(serde_json::json!("ok")),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let ctx = real_fs_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(
            !state_path.exists(),
            "State file should NOT be created for Bash without BROWSER_TEST_PASS"
        );
    }

    #[test]
    fn test_process_post_tool_records_on_cdp_marker() {
        let session_id = "test-post-tool-cdp";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_result: Some(serde_json::json!("Screenshot saved: C:\\tmp\\screenshot.png\nConsole errors: 0\n  No console errors detected\nBROWSER_TEST_PASS")),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let ctx = real_fs_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(
            has_recent_browser_test(&RealFsTest, session_id),
            "State file should be written after CDP test with BROWSER_TEST_PASS marker"
        );

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_process_post_tool_ignores_non_bash_non_browser_test_marker() {
        let session_id = "test-post-tool-read";
        let state_path = state_file_path(&RealFsTest, session_id).expect("concrete session id");
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_result: Some(serde_json::json!("BROWSER_TEST_PASS")),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let ctx = real_fs_ctx();
        let output = process_post_tool(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(
            !state_path.exists(),
            "State file should NOT be created for Read tool even with marker"
        );
    }
}
