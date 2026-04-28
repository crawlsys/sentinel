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
            return GitResult::Executed(self.executor.exec(&real_git, args));
        }

        // Evaluate policy
        let args_joined = args.join(" ");
        match interceptor::evaluate_git_command(&args_joined) {
            InterceptorPolicy::Allow => GitResult::Executed(self.executor.exec(&real_git, args)),
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
                        GitResult::Executed(self.executor.exec(&real_git, args))
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
            GitResult::Executed(self.executor.exec(real_git, args))
        } else {
            eprintln!("\x1b[0;31mDeclined\x1b[0m");
            GitResult::Declined
        }
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
