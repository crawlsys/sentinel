//! Project-scope tracker — publish the session's CURRENT working project so the
//! hookdeck channel router can scope incoming webhooks to the right session.
//!
//! **The problem this solves.** A session is launched from the operator's home
//! dir (`c` from `~`) and only *later* starts working on a specific repo — and
//! that repo changes as work moves. The hookdeck MCP child, spawned once at
//! session start with a fixed environment, cannot see those later `cd`s / file
//! edits, so it can't know which project the session is currently working on.
//! Without a live signal it falls back to the launch cwd basename (`garys` for a
//! home launch), which matches no webhook — so GitHub events are dropped and
//! sibling home sessions "bleed" into each other.
//!
//! **The fix.** On every `PostToolUse`, resolve the git repo root of whatever
//! the tool just operated on (a Bash command's cwd, or an Edit/Write/Read file
//! path) and write the repo name to a per-session file the channel router reads:
//! `~/.vulcan/hookdeck/session-<key>.project`. Stickiness is *latest wins* — the
//! project follows the most recent repo touched, so scope tracks current focus
//! and re-scopes instantly on a repo switch. When the tool operated somewhere
//! with no git repo above it (the home dir, a scratch path), the file is CLEARED
//! so the session resolves to `None` → the router's existing unscoped fail-open
//! delivers everything (never blind before you've started working anywhere).
//!
//! **Why the key matches vulcan.** The filename is keyed on the basename of
//! `CLAUDE_CONFIG_DIR`, which is exactly the third fallback in vulcan-hookdeck's
//! `resolve_session_key` (`VULCAN_SESSION_ID` / `CLAUDE_SESSION_ID` /
//! `CLAUDE_CONFIG_DIR` basename). Both the sentinel hook and the MCP child are
//! spawned by Claude Code in the same session environment, so they derive the
//! identical key with no handler involvement.
//!
//! Observational only — always `allow()`; a write failure never blocks a turn.

use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookInput, HookOutput};

/// Tools whose activity signals "the operator is working in this repo now".
/// Bash carries a cwd; the file tools carry a `file_path`.
fn is_tracked_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Bash" | "Edit" | "Write" | "Read" | "NotebookEdit" | "MultiEdit"
    )
}

/// Pick the filesystem path this tool call operated on:
/// * file tools (`Edit`/`Write`/`Read`/…) → the `file_path`;
/// * `Bash` → the command's cwd (`input.cwd`), since a `cd`'d command runs there.
///
/// Returns `None` when there's no usable path (so we neither update nor clear —
/// a pathless tool call leaves the current scope untouched).
fn operated_path<'a>(input: &'a HookInput, tool: &str) -> Option<&'a str> {
    match tool {
        "Bash" => input.cwd.as_deref(),
        // Edit/Write/Read/MultiEdit/NotebookEdit all carry file_path (2.1.89+);
        // fall back to cwd if a client omits it.
        _ => input.file_path.as_deref().or(input.cwd.as_deref()),
    }
    .map(str::trim)
    .filter(|s| !s.is_empty())
}

/// The per-session project file the hookdeck channel router reads. Lives beside
/// the session lockfiles in `~/.vulcan/hookdeck/`. Keyed on the sanitized
/// `CLAUDE_CONFIG_DIR` basename so it matches vulcan's `resolve_session_key`.
fn session_project_file(home: &Path, session_key: &str) -> PathBuf {
    home.join(".vulcan")
        .join("hookdeck")
        .join(format!("session-{session_key}.project"))
}

/// Derive the session key from `CLAUDE_CONFIG_DIR`'s basename — the same value
/// vulcan-hookdeck falls back to. Sanitized to a single path component so it
/// can't escape the hookdeck dir.
fn session_key(ctx: &super::HookContext<'_>) -> Option<String> {
    let dir = ctx.env.var("CLAUDE_CONFIG_DIR")?;
    let base = Path::new(dir.trim())
        .file_name()?
        .to_string_lossy()
        .to_string();
    let base = base.trim();
    if base.is_empty() || base.contains(['/', '\\']) {
        return None;
    }
    Some(base.to_string())
}

/// `PostToolUse` entry point. Fast + best-effort: resolve the repo the tool
/// touched and publish it (or clear the file for a non-repo path). Always allows.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let Some(tool) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };
    if !is_tracked_tool(tool) {
        return HookOutput::allow();
    }
    let Some(path) = operated_path(input, tool) else {
        return HookOutput::allow();
    };
    let Some(home) = ctx.fs.home_dir() else {
        return HookOutput::allow();
    };
    let Some(key) = session_key(ctx) else {
        return HookOutput::allow();
    };
    let file = session_project_file(&home, &key);

    // Resolve the git repo root of the operated path, then take the repo name.
    // No repo above the path (home dir / scratch) → clear the file so the
    // session resolves to None (router fail-open = receive everything).
    match ctx
        .git
        .repo_root(path)
        .as_deref()
        .and_then(repo_name_from_root)
    {
        Some(project) => {
            // Latest-wins, debounced: only rewrite when the project changed, to
            // avoid churning the file (and its mtime) on every tool call.
            let current = ctx.fs.read_to_string(&file).ok();
            if current.as_deref().map(str::trim) != Some(project.as_str()) {
                if let Some(parent) = file.parent() {
                    let _ = ctx.fs.create_dir_all(parent);
                }
                let _ = ctx.fs.write(&file, project.as_bytes());
                tracing::debug!(project = %project, key = %key, "project-scope updated");
            }
        }
        None => {
            // Non-repo path — clear scope so the session is unscoped (fail-open).
            if ctx.fs.exists(&file) {
                let _ = ctx.fs.write(&file, b"");
                tracing::debug!(key = %key, path, "project-scope cleared (no git repo)");
            }
        }
    }

    HookOutput::allow()
}

/// Repo name = basename of the git repo root. `repo_root` returns the absolute
/// path of the dir containing `.git`; the last component is the repo name the
/// GitHub webhook payload's `repository.full_name` also ends with.
fn repo_name_from_root(root: &str) -> Option<String> {
    let name = Path::new(root.trim())
        .file_name()?
        .to_string_lossy()
        .to_string();
    let name = name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, StubEnv, TestHomeFs};
    use crate::hooks::{EnvPort, FileSystemPort, GitStatusPort, HookContext};
    use sentinel_domain::events::HookInput;

    #[test]
    fn repo_name_is_basename_of_root() {
        assert_eq!(
            repo_name_from_root(r"C:\Users\garys\Documents\GitHub\hookdeck-mcp-rust").as_deref(),
            Some("hookdeck-mcp-rust")
        );
        assert_eq!(
            repo_name_from_root("/home/x/Documents/GitHub/memory-cli-rust").as_deref(),
            Some("memory-cli-rust")
        );
        assert_eq!(repo_name_from_root("   ").as_deref(), None);
    }

    #[test]
    fn only_tracked_tools_count() {
        assert!(is_tracked_tool("Bash"));
        assert!(is_tracked_tool("Edit"));
        assert!(is_tracked_tool("Write"));
        assert!(is_tracked_tool("Read"));
        assert!(!is_tracked_tool("TodoWrite"));
        assert!(!is_tracked_tool("Skill"));
    }

    #[test]
    fn operated_path_prefers_file_path_for_edit_and_cwd_for_bash() {
        let mut edit = HookInput {
            tool_name: Some("Edit".into()),
            file_path: Some(r"C:\repo\src\main.rs".into()),
            cwd: Some(r"C:\somewhere".into()),
            ..Default::default()
        };
        assert_eq!(operated_path(&edit, "Edit"), Some(r"C:\repo\src\main.rs"));
        edit.file_path = None;
        assert_eq!(operated_path(&edit, "Edit"), Some(r"C:\somewhere"));

        let bash = HookInput {
            tool_name: Some("Bash".into()),
            cwd: Some(r"C:\repo".into()),
            file_path: Some(r"C:\ignored".into()),
            ..Default::default()
        };
        assert_eq!(operated_path(&bash, "Bash"), Some(r"C:\repo"));
    }

    #[test]
    fn session_project_file_lives_in_hookdeck_dir() {
        let f = session_project_file(Path::new("/home/x"), "claude7-49788-123");
        assert!(f.ends_with("session-claude7-49788-123.project"));
        assert!(f.to_string_lossy().contains(".vulcan"));
        assert!(f.to_string_lossy().contains("hookdeck"));
    }

    #[test]
    fn writes_repo_name_for_a_repo_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        // git.repo_root stub returns a fixed repo root; see stub below.
        let env = super::super::test_support::StubEnv::with(&[(
            "CLAUDE_CONFIG_DIR",
            r"C:\Users\garys\.claude\session-env\claude7-49788-1783137048349",
        )]);
        let git = RepoRootStub {
            root: Some(r"C:\Users\garys\Documents\GitHub\hookdeck-mcp-rust".into()),
        };
        let ctx = stub_ctx_with_fs_env_git(&fs, &env, &git);
        let input = HookInput {
            tool_name: Some("Edit".into()),
            file_path: Some(r"C:\Users\garys\Documents\GitHub\hookdeck-mcp-rust\src\lib.rs".into()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(out.blocked.is_none());
        let file = session_project_file(tmp.path(), "claude7-49788-1783137048349");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hookdeck-mcp-rust");
    }

    #[test]
    fn clears_scope_for_home_path_with_no_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let env = super::super::test_support::StubEnv::with(&[(
            "CLAUDE_CONFIG_DIR",
            "claude7-49788-1783137048349",
        )]);
        // Pre-seed a stale scope file.
        let file = session_project_file(tmp.path(), "claude7-49788-1783137048349");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "old-project").unwrap();

        let git = RepoRootStub { root: None }; // home dir → no repo root
        let ctx = stub_ctx_with_fs_env_git(&fs, &env, &git);
        let input = HookInput {
            tool_name: Some("Bash".into()),
            cwd: Some(r"C:\Users\garys".into()),
            ..Default::default()
        };
        process(&input, &ctx);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
    }

    // --- test doubles ---

    struct RepoRootStub {
        root: Option<String>,
    }
    impl GitStatusPort for RepoRootStub {
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
            Ok(Vec::new())
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
            self.root.clone()
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
            None
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

    /// Build a HookContext with our fs + env + git, defaulting the rest from the
    /// standard stub (same field-by-field pattern as `stub_ctx_with_fs`).
    fn stub_ctx_with_fs_env_git<'a>(
        fs: &'a dyn FileSystemPort,
        env: &'a dyn EnvPort,
        git: &'a dyn GitStatusPort,
    ) -> HookContext<'a> {
        let base = stub_ctx_with_fs(fs);
        HookContext {
            git,
            vector_store: base.vector_store,
            fs,
            process: base.process,
            llm: base.llm,
            memory_mcp: base.memory_mcp,
            env,
            linear_lookup: base.linear_lookup,
        }
    }
}
