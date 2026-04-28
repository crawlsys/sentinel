//! Interceptor domain — pure policy logic for command interception.
//!
//! Covers two interceptor types:
//! - **Git**: blocks dangerous commands, offers safe alternatives, supports bypass
//! - **Npx**: redirects Node package commands to local Rust CLI equivalents

use std::collections::HashMap;

// ============================================================================
// Core types
// ============================================================================

/// Policy decision from evaluating a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterceptorPolicy {
    /// Command is safe — pass through to the real binary.
    Allow,
    /// Command is blocked — show reason and alternatives.
    Block {
        reason: String,
        alternatives: Vec<String>,
        risk: RiskLevel,
    },
    /// Command requires confirmation (e.g. --force flag in interactive mode).
    Confirm { risk: RiskLevel },
    /// Redirect to a different binary (npx → rust CLI).
    Redirect { target: String },
}

/// Risk classification for blocked/dangerous commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
            Self::Critical => "CRITICAL",
        }
    }
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ============================================================================
// Git policy evaluation — pure functions, no IO
// ============================================================================

/// Git-specific policy rule.
struct GitRule {
    pattern: &'static str,
    reason: &'static str,
    risk: RiskLevel,
    alternatives: &'static [&'static str],
}

const GIT_BLOCKED_RULES: &[GitRule] = &[
    GitRule {
        pattern: "clean -f",
        reason: "Permanently deletes untracked files",
        risk: RiskLevel::High,
        alternatives: &[
            "git clean -n          # Preview what would be deleted",
            "git clean -i          # Interactive clean",
            "git status --ignored  # See ignored files",
        ],
    },
    GitRule {
        pattern: "clean --force",
        reason: "Permanently deletes untracked files",
        risk: RiskLevel::High,
        alternatives: &[
            "git clean -n          # Preview what would be deleted",
            "git clean -i          # Interactive clean",
        ],
    },
    GitRule {
        pattern: "reset --hard",
        reason: "Discards ALL uncommitted changes",
        risk: RiskLevel::High,
        alternatives: &[
            "git stash             # Save changes temporarily",
            "git revert <commit>   # Undo commit safely",
            "git reset --soft      # Undo commit, keep changes",
            "git checkout -- file  # Restore specific file",
        ],
    },
    GitRule {
        pattern: "push --force",
        reason: "Overwrites remote history",
        risk: RiskLevel::Critical,
        alternatives: &[
            "git pull --rebase     # Sync with remote first",
            "git pull && git push  # Merge then push",
        ],
    },
    GitRule {
        pattern: "push -f",
        reason: "Overwrites remote history",
        risk: RiskLevel::Critical,
        alternatives: &[
            "git pull --rebase     # Sync with remote first",
            "git pull && git push  # Merge then push",
        ],
    },
    GitRule {
        pattern: "--force-with-lease",
        reason: "Overwrites remote with safety check",
        risk: RiskLevel::High,
        alternatives: &[
            "git pull --rebase     # Sync with remote first",
            "git pull && git push  # Merge then push",
        ],
    },
    GitRule {
        pattern: "rebase -i",
        reason: "Rewrites commit history",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git merge <branch>    # Merge instead of rebase",
            "git log --oneline     # Review commits first",
        ],
    },
    GitRule {
        pattern: "rebase --interactive",
        reason: "Rewrites commit history",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git merge <branch>    # Merge instead of rebase",
            "git log --oneline     # Review commits first",
        ],
    },
    GitRule {
        pattern: "rebase --onto",
        reason: "Rewrites commit history",
        risk: RiskLevel::Medium,
        alternatives: &["git merge <branch>    # Merge instead of rebase"],
    },
    GitRule {
        pattern: "filter-branch",
        reason: "Rewrites entire repo history",
        risk: RiskLevel::Critical,
        alternatives: &[],
    },
    GitRule {
        pattern: "filter-repo",
        reason: "Rewrites entire repo history",
        risk: RiskLevel::Critical,
        alternatives: &[],
    },
    GitRule {
        pattern: "stash drop",
        reason: "Permanently deletes stash",
        risk: RiskLevel::High,
        alternatives: &[
            "git stash list        # See all stashes",
            "git stash pop         # Apply and remove safely",
            "git stash apply       # Apply without removing",
        ],
    },
    GitRule {
        pattern: "stash clear",
        reason: "Permanently deletes all stashes",
        risk: RiskLevel::High,
        alternatives: &[
            "git stash list        # See all stashes",
            "git stash pop         # Apply and remove safely",
        ],
    },
    GitRule {
        pattern: "checkout -f",
        reason: "Discards local changes",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git stash             # Save changes first",
            "git checkout <branch> # Normal checkout",
        ],
    },
    GitRule {
        pattern: "checkout --force",
        reason: "Discards local changes",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git stash             # Save changes first",
            "git checkout <branch> # Normal checkout",
        ],
    },
    GitRule {
        pattern: "gc --aggressive",
        reason: "Affects repository recovery",
        risk: RiskLevel::Medium,
        alternatives: &[],
    },
    GitRule {
        pattern: "reflog expire",
        reason: "Affects repository recovery",
        risk: RiskLevel::Medium,
        alternatives: &[],
    },
    GitRule {
        pattern: "switch -f",
        reason: "Discards local changes",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git stash             # Save changes first",
            "git switch <branch>   # Normal switch",
        ],
    },
    GitRule {
        pattern: "switch --force",
        reason: "Discards local changes",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git stash             # Save changes first",
            "git switch <branch>   # Normal switch",
        ],
    },
    GitRule {
        pattern: "switch --discard",
        reason: "Discards local changes",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git stash             # Save changes first",
            "git switch <branch>   # Normal switch",
        ],
    },
    GitRule {
        pattern: "restore -f",
        reason: "Force restores files",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git restore <file>    # Normal restore",
            "git stash             # Save changes first",
        ],
    },
    GitRule {
        pattern: "restore --force",
        reason: "Force restores files",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git restore <file>    # Normal restore",
            "git stash             # Save changes first",
        ],
    },
    GitRule {
        pattern: "push --mirror",
        reason: "Overwrites ENTIRE remote",
        risk: RiskLevel::Critical,
        alternatives: &[
            "git push origin <branch>  # Push specific branch",
            "git push --all            # Push all branches",
        ],
    },
    GitRule {
        pattern: "branch -D",
        reason: "Force deletes unmerged branch",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git branch -d <name>  # Delete merged branch",
            "git merge <branch>    # Merge first, then delete",
        ],
    },
    GitRule {
        pattern: "push --delete",
        reason: "Deletes from remote",
        risk: RiskLevel::High,
        alternatives: &[
            "git branch -d <name>  # Delete local only",
            "git fetch --prune     # Clean stale refs",
        ],
    },
    GitRule {
        pattern: "push origin :",
        reason: "Deletes from remote",
        risk: RiskLevel::High,
        alternatives: &[
            "git branch -d <name>  # Delete local only",
            "git fetch --prune     # Clean stale refs",
        ],
    },
    GitRule {
        pattern: "tag -f",
        reason: "Overwrites existing tag",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git tag <new-name>    # Create new tag",
            "git tag -d <name>     # Delete old tag first",
        ],
    },
    GitRule {
        pattern: "tag --force",
        reason: "Overwrites existing tag",
        risk: RiskLevel::Medium,
        alternatives: &[
            "git tag <new-name>    # Create new tag",
            "git tag -d <name>     # Delete old tag first",
        ],
    },
];

/// Evaluate a git command against safety rules.
///
/// Returns `Allow` if safe, `Block` if dangerous, or `Confirm` if --force
/// is present but no specific rule matched (interactive confirmation needed).
pub fn evaluate_git_command(args_joined: &str) -> InterceptorPolicy {
    // Exception: prune is ok when used with `worktree prune`
    let is_prune = args_joined.contains("prune") && !args_joined.contains("worktree prune");

    // Check all blocked rules
    for rule in GIT_BLOCKED_RULES {
        if args_joined.contains(rule.pattern) {
            return InterceptorPolicy::Block {
                reason: rule.reason.to_string(),
                alternatives: rule.alternatives.iter().map(|s| (*s).to_string()).collect(),
                risk: rule.risk,
            };
        }
    }

    // Special case: prune without worktree context
    if is_prune {
        return InterceptorPolicy::Block {
            reason: "Affects repository recovery".to_string(),
            alternatives: vec![],
            risk: RiskLevel::Medium,
        };
    }

    // --force flag without a specific rule → needs confirmation
    if args_joined.contains("--force") {
        return InterceptorPolicy::Confirm {
            risk: RiskLevel::Medium,
        };
    }

    InterceptorPolicy::Allow
}

/// Classify risk for bypass display (used after a Block decision).
pub fn classify_risk(args_joined: &str) -> (RiskLevel, &'static str) {
    if args_joined.contains("clean -f") || args_joined.contains("clean --force") {
        (RiskLevel::High, "Permanently deletes untracked files")
    } else if args_joined.contains("reset --hard") {
        (RiskLevel::High, "Discards ALL uncommitted changes")
    } else if args_joined.contains("push --force") || args_joined.contains("push -f") {
        (RiskLevel::Critical, "Overwrites remote history")
    } else if args_joined.contains("--force-with-lease") {
        (RiskLevel::High, "Overwrites remote with safety check")
    } else if args_joined.contains("rebase -i") {
        (RiskLevel::Medium, "Rewrites commit history")
    } else if args_joined.contains("filter-branch") || args_joined.contains("filter-repo") {
        (RiskLevel::Critical, "Rewrites entire repo history")
    } else if args_joined.contains("stash drop") || args_joined.contains("stash clear") {
        (RiskLevel::High, "Permanently deletes stash")
    } else if args_joined.contains("checkout -f") || args_joined.contains("switch -f") {
        (RiskLevel::Medium, "Discards local changes")
    } else if args_joined.contains("push --mirror") {
        (RiskLevel::Critical, "Overwrites ENTIRE remote")
    } else if args_joined.contains("branch -D") {
        (RiskLevel::Medium, "Force deletes unmerged branch")
    } else if args_joined.contains("push --delete") {
        (RiskLevel::High, "Deletes from remote")
    } else if args_joined.contains("tag -f") || args_joined.contains("tag --force") {
        (RiskLevel::Medium, "Overwrites existing tag")
    } else {
        (RiskLevel::Medium, "Dangerous git operation")
    }
}

// ============================================================================
// Npx redirect evaluation — pure functions, no IO
// ============================================================================

/// Resolve an npx package name to a local Rust CLI binary.
pub fn resolve_npx_redirect(package: &str, redirects: &HashMap<String, String>) -> Option<String> {
    redirects.get(package).cloned()
}

/// Default npx → Rust CLI redirect table.
pub fn default_npx_redirects() -> HashMap<String, String> {
    let mut m = HashMap::new();
    let entries = [
        ("vercel", "vercel-cli-rs"),
        ("sanity", "sanity-cli-rs"),
        ("twilio", "twilio-cli-rs"),
        ("sendgrid", "sendgrid-cli-rs"),
        ("sentry-cli", "sentry-cli-rs"),
        ("@sentry/cli", "sentry-cli-rs"),
        ("doppler", "doppler-cli-rs"),
        ("neonctl", "neon-cli-rs"),
        ("@neondatabase/cli", "neon-cli-rs"),
        ("auth0", "auth0-cli-rs"),
        ("slack", "slack-cli-rs"),
        ("@slack/cli", "slack-cli-rs"),
        ("ringcentral", "ringcentral-cli-rs"),
        ("gh", "gh-cli-rs"),
        ("@cli/gh", "gh-cli-rs"),
        ("wrangler", "wrangler-rs"),
        ("playwright", "playwright-cli-rs"),
        ("@playwright/test", "playwright-cli-rs"),
        ("hubspot", "hubspot-cli-rs"),
        ("hs", "hubspot-cli-rs"),
        ("axiom", "axiom"),
        ("linear", "linear"),
        ("netlify", "netlify"),
        ("ntl", "netlify"),
        ("gtm", "google-gtm-cli-rs"),
        ("google-gtm", "google-gtm-cli-rs"),
    ];
    for (k, v) in entries {
        m.insert(k.to_string(), v.to_string());
    }
    m
}

// ============================================================================
// Shared utility — pure
// ============================================================================

/// Check if a cwd path is inside a Flutter SDK (exception for git interceptor).
pub fn is_flutter_sdk_path(cwd: &str) -> bool {
    let normalized = cwd.to_lowercase().replace('\\', "/");
    normalized.contains("dev/flutter") || normalized.contains("flutter/bin")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_safe_commands() {
        assert_eq!(evaluate_git_command("status"), InterceptorPolicy::Allow);
        assert_eq!(
            evaluate_git_command("commit -m test"),
            InterceptorPolicy::Allow
        );
        assert_eq!(
            evaluate_git_command("push origin main"),
            InterceptorPolicy::Allow
        );
        assert_eq!(
            evaluate_git_command("pull --rebase"),
            InterceptorPolicy::Allow
        );
        assert_eq!(
            evaluate_git_command("log --oneline"),
            InterceptorPolicy::Allow
        );
    }

    #[test]
    fn test_block_reset_hard() {
        match evaluate_git_command("reset --hard HEAD~1") {
            InterceptorPolicy::Block { risk, .. } => assert_eq!(risk, RiskLevel::High),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_block_force_push() {
        match evaluate_git_command("push --force origin main") {
            InterceptorPolicy::Block { risk, .. } => assert_eq!(risk, RiskLevel::Critical),
            other => panic!("expected Block, got {other:?}"),
        }
        match evaluate_git_command("push -f origin main") {
            InterceptorPolicy::Block { risk, .. } => assert_eq!(risk, RiskLevel::Critical),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_block_clean() {
        match evaluate_git_command("clean -fd") {
            InterceptorPolicy::Block {
                risk, alternatives, ..
            } => {
                assert_eq!(risk, RiskLevel::High);
                assert!(!alternatives.is_empty());
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_block_filter_branch() {
        match evaluate_git_command("filter-branch --all") {
            InterceptorPolicy::Block { risk, .. } => assert_eq!(risk, RiskLevel::Critical),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_block_stash_drop() {
        match evaluate_git_command("stash drop stash@{0}") {
            InterceptorPolicy::Block { risk, .. } => assert_eq!(risk, RiskLevel::High),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_block_branch_force_delete() {
        match evaluate_git_command("branch -D feature-x") {
            InterceptorPolicy::Block { risk, .. } => assert_eq!(risk, RiskLevel::Medium),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_confirm_generic_force() {
        match evaluate_git_command("worktree remove path --force") {
            InterceptorPolicy::Confirm { risk } => assert_eq!(risk, RiskLevel::Medium),
            other => panic!("expected Confirm, got {other:?}"),
        }
    }

    #[test]
    fn test_worktree_prune_allowed() {
        assert_eq!(
            evaluate_git_command("worktree prune"),
            InterceptorPolicy::Allow,
        );
    }

    #[test]
    fn test_plain_prune_blocked() {
        match evaluate_git_command("prune") {
            InterceptorPolicy::Block { .. } => {}
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn test_npx_redirect_found() {
        let redirects = default_npx_redirects();
        assert_eq!(
            resolve_npx_redirect("vercel", &redirects),
            Some("vercel-cli-rs".to_string()),
        );
        assert_eq!(
            resolve_npx_redirect("@sentry/cli", &redirects),
            Some("sentry-cli-rs".to_string()),
        );
    }

    #[test]
    fn test_npx_redirect_not_found() {
        let redirects = default_npx_redirects();
        assert_eq!(resolve_npx_redirect("unknown-package", &redirects), None);
    }

    #[test]
    fn test_flutter_exception() {
        assert!(is_flutter_sdk_path("/home/user/dev/flutter/bin"));
        assert!(is_flutter_sdk_path(r"C:\Users\gary\dev\flutter\packages"));
        assert!(!is_flutter_sdk_path("/home/user/projects/my-app"));
    }

    #[test]
    fn test_risk_level_ordering() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }

    #[test]
    fn test_default_redirects_count() {
        let r = default_npx_redirects();
        assert!(
            r.len() >= 20,
            "expected at least 20 redirects, got {}",
            r.len()
        );
    }
}
