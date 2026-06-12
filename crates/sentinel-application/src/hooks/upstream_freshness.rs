//! Upstream Freshness — notify (never mutate) when local `main` is behind
//! `origin/main`, so you don't unknowingly work on stale code.
//!
//! Motivation: a checkout's local default branch can silently fall behind its
//! remote (e.g. a second machine that hasn't pulled since the last push).
//! Working on stale `main` wastes effort and invites merge surprises. The
//! existing hourly Git Hygiene cron covers the *opposite* direction (local
//! commits not yet pushed UP); this hook covers "you're behind, pull DOWN".
//!
//! Design constraints (deliberate, per the approved shape):
//!   - **Notify + one-click, never auto-mutate.** The hook only injects an
//!     advisory naming the exact safe command (`git pull --ff-only origin
//!     <branch>`). It does NOT run git in a mutating mode — no clobbering
//!     uncommitted work, no mid-session merge/rebase surprises, no working-tree
//!     changes under an active session. The user stays in control.
//!   - **Zero network.** It compares HEAD to the *already-fetched* `origin/<b>`
//!     ref via `rev_list_count` — no `git fetch`, so SessionStart/CwdChanged
//!     stay fast and can't fail on auth/latency. Freshness of `origin/<b>` is
//!     only as good as the last fetch; the hourly cron does the fetch-backed
//!     authoritative check off the critical path.
//!   - **Default branch only.** Only nudges on `main`/`master`. Feature
//!     branches are intentionally left alone — pulling onto them is rarely the
//!     safe FF case and the user manages those explicitly.
//!   - **Fail silent.** Not a repo, no upstream ref, detached HEAD, branch
//!     unresolved, or up-to-date → `allow()` with no output. The hook is a
//!     gentle nudge, never noise.

use sentinel_domain::events::{HookEnvelope, HookEvent, HookInput, HookOutput};

use super::{GitStatusPort, HookContext};

/// Branches we treat as the shared default branch worth nudging about.
const DEFAULT_BRANCHES: &[&str] = &["main", "master"];

/// Compute the freshness notice for the repo containing `cwd`, if any.
///
/// Returns `Some(message)` when local default branch is behind its
/// already-fetched `origin/<branch>` ref; `None` in every silent case
/// (not a repo, not on default branch, no upstream ref, up-to-date).
fn freshness_notice(git: &dyn GitStatusPort, cwd: &str) -> Option<String> {
    // Must be inside a git repo.
    let repo = git.repo_root(cwd)?;

    // Only nudge on the shared default branch — feature branches are the
    // user's to manage; FF-pulling onto them is rarely the safe case.
    let branch = git.current_branch(&repo).ok()?;
    if !DEFAULT_BRANCHES.contains(&branch.as_str()) {
        return None;
    }

    // Count commits on the remote that we don't have locally, using the
    // already-fetched ref — NO network. `HEAD..origin/<branch>` = commits
    // reachable from the remote ref but not from HEAD, i.e. how far behind we
    // are. Must use rev_list_count_range (verbatim range): plain
    // rev_list_count appends `..HEAD`, which would mangle this into an invalid
    // `HEAD..origin/<b>..HEAD`. `None`/0 → nothing to say (ref unresolved or
    // current).
    let upstream = format!("origin/{branch}");
    let behind = git.rev_list_count_range(&repo, &format!("HEAD..{upstream}"))?;
    if behind == 0 {
        return None;
    }

    // We're behind. Decide whether a fast-forward pull is clean. A dirty tree
    // means a pull could conflict / stash-dance, so we still notify but steer
    // the user to review first rather than naming a one-click command.
    let dirty = git.has_uncommitted_changes(&repo).unwrap_or(false);
    let commits = if behind == 1 { "commit" } else { "commits" };

    let msg = if dirty {
        format!(
            "⬇️  `{repo}` is {behind} {commits} behind `{upstream}`, but the working \
             tree has uncommitted changes. Review/commit/stash first, then \
             `git pull --ff-only origin {branch}`."
        )
    } else {
        format!(
            "⬇️  `{repo}` is {behind} {commits} behind `{upstream}` and the tree is clean \
             (fast-forward safe). One-click to get current:\n\
             `git pull --ff-only origin {branch}`"
        )
    };
    Some(msg)
}

/// Process `SessionStart` / `CwdChanged` — inject an upstream-freshness notice
/// when the local default branch is behind origin. `event` is the triggering
/// event so the injected context is tagged correctly. Notify-only; never
/// mutates the repo.
pub fn process(input: &HookInput, ctx: &HookContext<'_>, event: HookEvent) -> HookOutput {
    // On CwdChanged the relevant path is the *new* cwd; on SessionStart it's
    // `input.cwd`. Prefer the explicit new_cwd when present (CwdChanged), else
    // fall back to cwd.
    let cwd = input
        .extra
        .get("new_cwd")
        .and_then(|v| v.as_str())
        .or(input.cwd.as_deref())
        .unwrap_or(".");

    match freshness_notice(ctx.git, cwd) {
        Some(message) => {
            let envelope = HookEnvelope::info("Upstream Freshness", message);
            HookOutput::inject_envelope(event, &envelope)
        }
        None => HookOutput::allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::stub_ctx;
    use std::path::PathBuf;

    /// Configurable git stub for freshness scenarios.
    struct FreshGit {
        repo: Option<String>,
        branch: String,
        behind: Option<u32>,
        dirty: bool,
    }
    impl Default for FreshGit {
        fn default() -> Self {
            Self {
                repo: Some("/repo".to_string()),
                branch: "main".to_string(),
                behind: Some(0),
                dirty: false,
            }
        }
    }
    impl GitStatusPort for FreshGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            Ok(self.dirty)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> {
            Ok(self.branch.clone())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            self.repo.clone()
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            vec![]
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            // Not used by this hook; behind-count comes via the range method.
            Some(0)
        }
        fn rev_list_count_range(&self, _: &str, range: &str) -> Option<u32> {
            // The hook asks for "HEAD..origin/<branch>"; return the configured
            // behind count for that shape, else 0.
            if range.starts_with("HEAD..origin/") {
                self.behind
            } else {
                Some(0)
            }
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            vec![]
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            vec![]
        }
    }

    fn notice(git: &FreshGit) -> Option<String> {
        freshness_notice(git, "/repo/some/dir")
    }

    #[test]
    fn behind_on_main_clean_offers_one_click() {
        let n = notice(&FreshGit {
            behind: Some(3),
            ..Default::default()
        })
        .expect("should notify when behind");
        assert!(n.contains("3 commits behind"));
        assert!(n.contains("git pull --ff-only origin main"));
        assert!(n.contains("fast-forward safe"));
    }

    #[test]
    fn behind_singular_commit_grammar() {
        let n = notice(&FreshGit {
            behind: Some(1),
            ..Default::default()
        })
        .unwrap();
        assert!(n.contains("1 commit behind"), "singular grammar: {n}");
    }

    #[test]
    fn behind_but_dirty_steers_to_review() {
        let n = notice(&FreshGit {
            behind: Some(2),
            dirty: true,
            ..Default::default()
        })
        .expect("should still notify when dirty");
        assert!(n.contains("uncommitted changes"));
        assert!(n.contains("Review"));
        // Must NOT present it as a clean one-click.
        assert!(!n.contains("fast-forward safe"));
    }

    #[test]
    fn up_to_date_is_silent() {
        assert!(notice(&FreshGit {
            behind: Some(0),
            ..Default::default()
        })
        .is_none());
    }

    #[test]
    fn feature_branch_is_silent() {
        assert!(notice(&FreshGit {
            branch: "feat/whatever".to_string(),
            behind: Some(5),
            ..Default::default()
        })
        .is_none());
    }

    #[test]
    fn master_is_also_a_default_branch() {
        let n = notice(&FreshGit {
            branch: "master".to_string(),
            behind: Some(1),
            ..Default::default()
        });
        assert!(n.is_some(), "master should be treated as default branch");
    }

    #[test]
    fn not_a_repo_is_silent() {
        assert!(notice(&FreshGit {
            repo: None,
            behind: Some(9),
            ..Default::default()
        })
        .is_none());
    }

    #[test]
    fn no_upstream_ref_is_silent() {
        // rev_list_count returns None when origin/main doesn't resolve.
        assert!(notice(&FreshGit {
            behind: None,
            ..Default::default()
        })
        .is_none());
    }

    #[test]
    fn process_injects_on_sessionstart_when_behind() {
        let base = stub_ctx();
        let git = FreshGit {
            behind: Some(2),
            ..Default::default()
        };
        let ctx = HookContext {
            git: &git,
            vector_store: None,
            fs: base.fs,
            process: base.process,
            llm: None,
            memory_mcp: base.memory_mcp,
            env: base.env,
            linear_lookup: None,
        };
        let mut input = HookInput::default();
        input.cwd = Some("/repo".to_string());
        let out = process(&input, &ctx, HookEvent::SessionStart);
        let injected = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("context injected");
        assert!(injected.contains("Upstream Freshness"));
        assert!(injected.contains("git pull --ff-only origin main"));
    }

    #[test]
    fn process_prefers_new_cwd_on_cwd_changed() {
        let base = stub_ctx();
        let git = FreshGit {
            behind: Some(1),
            ..Default::default()
        };
        let ctx = HookContext {
            git: &git,
            vector_store: None,
            fs: base.fs,
            process: base.process,
            llm: None,
            memory_mcp: base.memory_mcp,
            env: base.env,
            linear_lookup: None,
        };
        let mut input = HookInput::default();
        input
            .extra
            .insert("new_cwd".to_string(), serde_json::json!("/repo/sub"));
        let out = process(&input, &ctx, HookEvent::CwdChanged);
        assert!(out.hook_specific_output.is_some(), "should inject on cwd change");
    }

    #[test]
    fn process_silent_when_current() {
        let base = stub_ctx();
        let git = FreshGit::default(); // behind 0
        let ctx = HookContext {
            git: &git,
            vector_store: None,
            fs: base.fs,
            process: base.process,
            llm: None,
            memory_mcp: base.memory_mcp,
            env: base.env,
            linear_lookup: None,
        };
        let mut input = HookInput::default();
        input.cwd = Some("/repo".to_string());
        let out = process(&input, &ctx, HookEvent::SessionStart);
        assert!(out.hook_specific_output.is_none(), "no output when current");
        assert!(out.blocked.is_none());
    }

    // Silence unused import warning for PathBuf in case future tests need it.
    #[allow(dead_code)]
    fn _uses_pathbuf() -> Option<PathBuf> {
        None
    }
}
