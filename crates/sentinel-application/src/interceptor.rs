//! Interceptor application services — orchestrate domain policy with IO ports.
//!
//! Provides `GitInterceptorService` and `NpxInterceptorService` that wire
//! pure domain evaluation to platform-specific adapters via port traits.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use sentinel_domain::interceptor::{self, InterceptorPolicy};

// ============================================================================
// Port traits — implemented by infrastructure adapters
// ============================================================================

/// Resolve the real binary for a given tool name, skipping our own path.
pub trait BinaryResolverPort {
    fn find_real_binary(&self, name: &str, self_path: &std::path::Path) -> Option<PathBuf>;
}

/// Show a bypass confirmation dialog to the human.
pub trait BypassDialogPort {
    fn show_bypass_dialog(&self, message: &str) -> bool;
}

/// Execute a binary with args and return the exit code.
pub trait ProcessExecutorPort {
    fn exec(&self, binary: &std::path::Path, args: &[String]) -> ExitCode;
}

/// Check if the current stdin is an interactive terminal.
pub trait InteractiveCheckPort {
    fn is_interactive(&self) -> bool;
}

/// Load npx redirect overrides from config.
pub trait RedirectLoaderPort {
    fn load_overrides(&self) -> HashMap<String, String>;
}

// ============================================================================
// Git interceptor service
// ============================================================================

/// Orchestrates git command interception: evaluate → bypass/block → exec.
pub struct GitInterceptorService<'a> {
    resolver: &'a dyn BinaryResolverPort,
    bypass: &'a dyn BypassDialogPort,
    executor: &'a dyn ProcessExecutorPort,
    interactive: &'a dyn InteractiveCheckPort,
}

/// Result of git interception.
pub enum GitResult {
    /// Executed the real git, here's the exit code.
    Executed(ExitCode),
    /// Command was blocked and not executed.
    Blocked,
    /// Bypass was declined by the user.
    Declined,
    /// Could not find the real git binary.
    BinaryNotFound,
}

impl<'a> GitInterceptorService<'a> {
    pub fn new(
        resolver: &'a dyn BinaryResolverPort,
        bypass: &'a dyn BypassDialogPort,
        executor: &'a dyn ProcessExecutorPort,
        interactive: &'a dyn InteractiveCheckPort,
    ) -> Self {
        Self {
            resolver,
            bypass,
            executor,
            interactive,
        }
    }

    /// Run the interceptor logic for a git command.
    pub fn run(&self, args: &[String], self_path: &std::path::Path, cwd: &str) -> GitResult {
        // Find real git
        let real_git = match self.resolver.find_real_binary("git", self_path) {
            Some(p) => p,
            None => return GitResult::BinaryNotFound,
        };

        // Check for --bypass flag
        if args.first().map(|s| s.as_str()) == Some("--bypass") {
            let rest: Vec<String> = args.iter().skip(1).cloned().collect();
            return self.handle_bypass(&rest, &real_git, cwd);
        }

        // Flutter SDK exception
        if interceptor::is_flutter_sdk_path(cwd) {
            let code = self.executor.exec(&real_git, args);
            post_exec_cleanup(args, cwd, &code);
            return GitResult::Executed(code);
        }

        // Evaluate policy. Use the args-aware entrypoint so commit message
        // bodies containing flag-like text ("--force", "reset --hard") don't
        // false-positive — only the actual flags on the command line matter.
        let args_joined = args.join(" ");
        match interceptor::evaluate_git_args(args) {
            InterceptorPolicy::Allow => {
                let code = self.executor.exec(&real_git, args);
                post_exec_cleanup(args, cwd, &code);
                GitResult::Executed(code)
            }
            InterceptorPolicy::Block {
                reason,
                alternatives,
                risk: _,
            } => {
                // Print block message to stderr
                eprintln!("\x1b[0;31mBLOCKED: {reason}\x1b[0m");
                if !alternatives.is_empty() {
                    eprintln!("\x1b[0;32mSafe alternatives:\x1b[0m");
                    for alt in &alternatives {
                        eprintln!("  {alt}");
                    }
                }
                eprintln!();
                eprintln!("\x1b[1;33mTo bypass with approval: git --bypass {args_joined}\x1b[0m");
                GitResult::Blocked
            }
            InterceptorPolicy::Confirm { risk: _ } => {
                if self.interactive.is_interactive() {
                    eprintln!("\x1b[1;33mWarning: --force\x1b[0m");
                    eprint!("Proceed? (yes/no): ");
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    let mut input = String::new();
                    if std::io::stdin().read_line(&mut input).is_ok() && input.trim() == "yes" {
                        let code = self.executor.exec(&real_git, args);
                        post_exec_cleanup(args, cwd, &code);
                        GitResult::Executed(code)
                    } else {
                        GitResult::Declined
                    }
                } else {
                    eprintln!("\x1b[0;31mBLOCKED: --force non-interactive\x1b[0m");
                    eprintln!(
                        "\x1b[1;33mTo bypass with approval: git --bypass {args_joined}\x1b[0m"
                    );
                    GitResult::Blocked
                }
            }
            InterceptorPolicy::Redirect { .. } => {
                // Git doesn't redirect — shouldn't happen
                GitResult::Executed(self.executor.exec(&real_git, args))
            }
        }
    }

    fn handle_bypass(&self, args: &[String], real_git: &std::path::Path, cwd: &str) -> GitResult {
        let args_joined = args.join(" ");
        let (risk, desc) = interceptor::classify_risk(&args_joined);

        let msg = format!(
            "COMMAND: git {args_joined}\n\n\
             DIRECTORY: {cwd}\n\n\
             RISK: {risk}\n\n\
             {desc}\n\n\
             Allow this operation?"
        );

        // Print bypass request header to stderr
        eprintln!();
        eprintln!("\x1b[1;33m---------------------------------------------------\x1b[0m");
        eprintln!("\x1b[0;31m  GIT SAFETY BYPASS REQUEST\x1b[0m");
        eprintln!("\x1b[1;33m---------------------------------------------------\x1b[0m");
        eprintln!("  \x1b[0;32mCommand:\x1b[0m   git {args_joined}");
        eprintln!("  \x1b[0;32mDirectory:\x1b[0m {cwd}");
        eprintln!("  \x1b[0;32mRisk:\x1b[0m      \x1b[0;31m{risk}\x1b[0m");
        eprintln!("  \x1b[0;32mInfo:\x1b[0m      {desc}");
        eprintln!("\x1b[1;33m---------------------------------------------------\x1b[0m");
        eprintln!();

        if self.bypass.show_bypass_dialog(&msg) {
            eprintln!("\x1b[0;32mApproved - executing...\x1b[0m");
            let code = self.executor.exec(real_git, args);
            post_exec_cleanup(args, cwd, &code);
            GitResult::Executed(code)
        } else {
            eprintln!("\x1b[0;31mDeclined\x1b[0m");
            GitResult::Declined
        }
    }
}

/// Post-exec hygiene: when the wrapped git command was `worktree remove`,
/// `git` itself unregisters the worktree from its admin state but on Windows
/// it routinely fails to delete the on-disk directory shell because some
/// process (file watcher, mcp-router, IDE) holds a handle. Git returns 0
/// regardless, leaving an orphaned dir that hygiene_reminders later flags.
///
/// This helper detects `worktree remove [--force] <path>`, and if the dir
/// still exists after git's exit, retries `std::fs::remove_dir_all` with
/// short backoff. Unix paths fall through to a single best-effort retry —
/// `git worktree remove` is reliable there but no harm in being defensive.
///
/// Failures are logged to stderr and otherwise ignored — we never fail the
/// overall command, since git already considers it successful.
fn post_exec_cleanup(args: &[String], cwd: &str, code: &ExitCode) {
    // Compare ExitCode by formatted string — `ExitCode` doesn't expose its raw
    // value, but `Debug` renders 0 as "ExitCode(unix_exit_status(0))" on Unix
    // and "ExitCode(ExitCode(0))" on Windows. The "(0)" substring is stable.
    let code_str = format!("{code:?}");
    if !code_str.contains("(0)") {
        return;
    }
    let Some(target) = parse_worktree_remove_target(args, cwd) else {
        return;
    };
    if !target.exists() {
        return;
    }
    let attempts = 3u32;
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(150 * u64::from(attempt)));
        }
        match std::fs::remove_dir_all(&target) {
            Ok(()) => {
                if attempt > 0 {
                    eprintln!(
                        "\x1b[0;32m[sentinel] Removed orphaned worktree dir on retry #{}: {}\x1b[0m",
                        attempt + 1,
                        target.display()
                    );
                }
                return;
            }
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(e) = last_err {
        // Treat NotFound as a benign race (another process beat us to it).
        if e.kind() == std::io::ErrorKind::NotFound {
            return;
        }
        eprintln!(
            "\x1b[1;33m[sentinel] worktree remove succeeded but on-disk shell could not be deleted ({}): {}\x1b[0m",
            target.display(),
            e
        );
    }
}

/// Parse `worktree remove [--force] <path>` and return the absolute path
/// to the worktree directory. Returns `None` for any other git command.
///
/// Tolerates flag ordering: `--force` may appear before or after the path.
/// Other args (long options like `--quiet`) are accepted and skipped.
fn parse_worktree_remove_target(args: &[String], cwd: &str) -> Option<PathBuf> {
    let mut iter = args.iter().peekable();
    if iter.next()?.as_str() != "worktree" {
        return None;
    }
    if iter.next()?.as_str() != "remove" {
        return None;
    }
    let mut path: Option<&str> = None;
    for arg in iter {
        if arg.starts_with('-') {
            continue;
        }
        path = Some(arg.as_str());
        break;
    }
    let path = path?;
    let p = PathBuf::from(path);
    Some(if p.is_absolute() {
        p
    } else {
        PathBuf::from(cwd).join(p)
    })
}

#[cfg(test)]
mod parse_worktree_remove_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_relative_path() {
        let got =
            parse_worktree_remove_target(&s(&["worktree", "remove", ".claude/wt/x"]), "/repo");
        assert_eq!(got.unwrap(), PathBuf::from("/repo").join(".claude/wt/x"));
    }

    #[test]
    fn parses_with_force_flag() {
        let got = parse_worktree_remove_target(
            &s(&["worktree", "remove", "--force", ".claude/wt/x"]),
            "/repo",
        );
        assert_eq!(got.unwrap(), PathBuf::from("/repo").join(".claude/wt/x"));
    }

    #[test]
    fn parses_path_before_flag() {
        // Some users pass `--force` after the path; git accepts both orders.
        let got = parse_worktree_remove_target(
            &s(&["worktree", "remove", ".claude/wt/x", "--force"]),
            "/repo",
        );
        assert_eq!(got.unwrap(), PathBuf::from("/repo").join(".claude/wt/x"));
    }

    #[test]
    fn parses_absolute_path_unchanged() {
        let got = parse_worktree_remove_target(&s(&["worktree", "remove", "/abs/path"]), "/repo");
        assert_eq!(got.unwrap(), PathBuf::from("/abs/path"));
    }

    #[test]
    fn ignores_non_worktree_commands() {
        assert!(parse_worktree_remove_target(&s(&["status"]), "/r").is_none());
        assert!(parse_worktree_remove_target(&s(&["worktree", "list"]), "/r").is_none());
        assert!(parse_worktree_remove_target(&s(&["worktree", "add", "x"]), "/r").is_none());
    }

    #[test]
    fn handles_empty_args() {
        assert!(parse_worktree_remove_target(&s(&[]), "/r").is_none());
        assert!(parse_worktree_remove_target(&s(&["worktree"]), "/r").is_none());
        assert!(parse_worktree_remove_target(&s(&["worktree", "remove"]), "/r").is_none());
    }
}

// ============================================================================
// Npx interceptor service
// ============================================================================

/// Orchestrates npx command interception: check redirects → redirect or passthrough.
pub struct NpxInterceptorService<'a> {
    resolver: &'a dyn BinaryResolverPort,
    executor: &'a dyn ProcessExecutorPort,
    redirect_loader: &'a dyn RedirectLoaderPort,
}

/// Result of npx interception.
pub enum NpxResult {
    /// Executed a binary (either redirect target or real npx).
    Executed(ExitCode),
    /// Could not find real npx.
    BinaryNotFound,
    /// Redirect target binary not found.
    RedirectNotFound(String),
}

impl<'a> NpxInterceptorService<'a> {
    pub fn new(
        resolver: &'a dyn BinaryResolverPort,
        executor: &'a dyn ProcessExecutorPort,
        redirect_loader: &'a dyn RedirectLoaderPort,
    ) -> Self {
        Self {
            resolver,
            executor,
            redirect_loader,
        }
    }

    /// Run the interceptor logic for an npx command.
    pub fn run(&self, args: &[String], self_path: &std::path::Path) -> NpxResult {
        // Build redirect table: defaults + overrides
        let mut redirects = interceptor::default_npx_redirects();
        let overrides = self.redirect_loader.load_overrides();
        redirects.extend(overrides);

        // Check if first arg matches a redirect
        if let Some(package) = args.first() {
            if let Some(target) = interceptor::resolve_npx_redirect(package, &redirects) {
                eprintln!();
                eprintln!("\x1b[1;33m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
                eprintln!("\x1b[0;31m  INTERCEPTED:\x1b[0m \x1b[1mnpx {package}\x1b[0m");
                eprintln!("\x1b[0;32m  Redirecting:\x1b[0m \x1b[0;36m\x1b[1m{target}\x1b[0m");
                eprintln!("\x1b[1;33m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
                eprintln!();

                let target_path = PathBuf::from(&target);
                let remaining: Vec<String> = args.iter().skip(1).cloned().collect();
                return NpxResult::Executed(self.executor.exec(&target_path, &remaining));
            }
        }

        // No redirect — find and execute real npx
        let real_npx = match self.resolver.find_real_binary("npx", self_path) {
            Some(p) => p,
            None => return NpxResult::BinaryNotFound,
        };

        NpxResult::Executed(self.executor.exec(&real_npx, args))
    }
}
