//! Interceptor infrastructure adapters — platform-specific IO implementations.
//!
//! Implements the port traits defined in `sentinel-application::interceptor`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sentinel_application::interceptor::{
    BinaryResolverPort, BypassDialogPort, InteractiveCheckPort, ProcessExecutorPort,
    RedirectLoaderPort,
};

// ============================================================================
// Binary resolver — finds real git/npx by searching known paths + PATH
// ============================================================================

pub struct PathBinaryResolver;

impl BinaryResolverPort for PathBinaryResolver {
    fn find_real_binary(&self, name: &str, self_path: &Path) -> Option<PathBuf> {
        // Platform-specific candidate paths
        let candidates = get_candidates(name);

        // Check candidates first
        for candidate in candidates.into_iter().flatten() {
            let path = PathBuf::from(&candidate);
            if path.exists() && path != self_path {
                return Some(path);
            }
        }

        // Fall back to searching PATH
        if let Ok(path_var) = std::env::var("PATH") {
            #[cfg(target_os = "windows")]
            let separator = ';';
            #[cfg(not(target_os = "windows"))]
            let separator = ':';

            let binary_names = get_binary_names(name);

            for dir in path_var.split(separator) {
                for bin_name in &binary_names {
                    let bin_path = PathBuf::from(dir).join(bin_name);
                    if bin_path.exists() && bin_path != self_path {
                        return Some(bin_path);
                    }
                }
            }
        }

        None
    }
}

fn get_candidates(name: &str) -> Vec<Option<String>> {
    match name {
        "git" => get_git_candidates(),
        "npx" => get_npx_candidates(),
        _ => vec![],
    }
}

fn get_binary_names(name: &str) -> Vec<&'static str> {
    match name {
        #[cfg(target_os = "windows")]
        "git" => vec!["git.exe"],
        #[cfg(not(target_os = "windows"))]
        "git" => vec!["git"],
        #[cfg(target_os = "windows")]
        "npx" => vec!["npx.cmd", "npx.exe", "npx.ps1"],
        #[cfg(not(target_os = "windows"))]
        "npx" => vec!["npx"],
        _ => vec![],
    }
}

#[cfg(target_os = "windows")]
fn get_git_candidates() -> Vec<Option<String>> {
    vec![
        Some(r"C:\Program Files\Git\bin\git.exe".to_string()),
        Some(r"C:\Program Files\Git\cmd\git.exe".to_string()),
        Some(r"C:\Program Files (x86)\Git\bin\git.exe".to_string()),
        std::env::var("LOCALAPPDATA")
            .ok()
            .map(|p| format!("{p}\\Programs\\Git\\bin\\git.exe")),
    ]
}

#[cfg(target_os = "macos")]
fn get_git_candidates() -> Vec<Option<String>> {
    vec![
        Some("/usr/bin/git".to_string()),
        Some("/usr/local/bin/git".to_string()),
        Some("/opt/homebrew/bin/git".to_string()),
        Some("/Library/Developer/CommandLineTools/usr/bin/git".to_string()),
    ]
}

#[cfg(target_os = "linux")]
fn get_git_candidates() -> Vec<Option<String>> {
    vec![
        Some("/usr/bin/git".to_string()),
        Some("/usr/local/bin/git".to_string()),
        Some("/bin/git".to_string()),
    ]
}

#[cfg(target_os = "windows")]
fn get_npx_candidates() -> Vec<Option<String>> {
    vec![
        Some(r"C:\Program Files\nodejs\npx.cmd".to_string()),
        Some(r"C:\Program Files (x86)\nodejs\npx.cmd".to_string()),
        std::env::var("APPDATA")
            .ok()
            .map(|p| format!("{p}\\npm\\npx.cmd")),
        std::env::var("LOCALAPPDATA")
            .ok()
            .map(|p| format!("{p}\\npm\\npx.cmd")),
    ]
}

#[cfg(target_os = "macos")]
fn get_npx_candidates() -> Vec<Option<String>> {
    vec![
        Some("/usr/local/bin/npx".to_string()),
        Some("/opt/homebrew/bin/npx".to_string()),
        std::env::var("HOME")
            .ok()
            .map(|p| format!("{p}/.nvm/current/bin/npx")),
        std::env::var("HOME")
            .ok()
            .map(|p| format!("{p}/.nodenv/shims/npx")),
    ]
}

#[cfg(target_os = "linux")]
fn get_npx_candidates() -> Vec<Option<String>> {
    vec![
        Some("/usr/bin/npx".to_string()),
        Some("/usr/local/bin/npx".to_string()),
        std::env::var("HOME")
            .ok()
            .map(|p| format!("{p}/.nvm/current/bin/npx")),
        std::env::var("HOME")
            .ok()
            .map(|p| format!("{p}/.nodenv/shims/npx")),
        std::env::var("HOME")
            .ok()
            .map(|p| format!("{p}/.local/bin/npx")),
    ]
}

// ============================================================================
// Bypass dialog — native OS confirmation dialogs
// ============================================================================

pub struct NativeBypassDialog;

impl BypassDialogPort for NativeBypassDialog {
    fn show_bypass_dialog(&self, message: &str) -> bool {
        #[cfg(target_os = "windows")]
        {
            let ps = format!(
                "Add-Type -AssemblyName PresentationFramework;\
                 [System.Windows.MessageBox]::Show('{}','Git Safety Bypass','YesNo','Warning')",
                message.replace('\'', "''")
            );
            let out = Command::new("powershell")
                .args(["-NoProfile", "-Command", &ps])
                .output();
            matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "Yes")
        }

        #[cfg(target_os = "macos")]
        {
            let script = format!(
                r#"display dialog "{}" buttons {{"No", "Yes"}} default button "No" with icon caution"#,
                message.replace('"', "\\\"")
            );
            let out = Command::new("osascript").args(["-e", &script]).output();
            matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("Yes"))
        }

        #[cfg(target_os = "linux")]
        {
            // Try zenity (GTK), then kdialog (KDE), then terminal
            let out = Command::new("zenity")
                .args(["--question", "--title=Git Safety Bypass", "--text", message])
                .status();
            if let Ok(status) = out {
                return status.success();
            }

            let out = Command::new("kdialog")
                .args(["--yesno", message, "--title", "Git Safety Bypass"])
                .status();
            if let Ok(status) = out {
                return status.success();
            }

            // Terminal fallback
            eprintln!("{message}");
            eprint!("Allow? (yes/no): ");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();
            input.trim().to_lowercase() == "yes"
        }
    }
}

// ============================================================================
// Process executor — spawn binary, forward args, return exit code
// ============================================================================

pub struct SystemProcessExecutor;

impl ProcessExecutorPort for SystemProcessExecutor {
    fn exec(&self, binary: &Path, args: &[String]) -> ExitCode {
        match Command::new(binary).args(args).status() {
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(e) => {
                eprintln!(
                    "\x1b[0;31mFailed to execute {}: {e}\x1b[0m",
                    binary.display()
                );
                ExitCode::from(1)
            }
        }
    }
}

// ============================================================================
// Interactive check — wraps a precomputed bool (TTY detection done in main)
// ============================================================================

/// Simple interactive check that wraps a precomputed value.
///
/// The actual TTY detection requires platform-specific `unsafe` FFI calls
/// (`GetConsoleMode` on Windows, `isatty` on Unix). Since the sentinel workspace
/// forbids `unsafe_code`, the binary crates perform the check in `main()` and
/// pass the result here.
pub struct PrecomputedInteractiveCheck {
    pub interactive: bool,
}

impl InteractiveCheckPort for PrecomputedInteractiveCheck {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
}

// ============================================================================
// TOML redirect loader — load npx overrides from config file
// ============================================================================

pub struct TomlRedirectLoader;

impl RedirectLoaderPort for TomlRedirectLoader {
    fn load_overrides(&self) -> HashMap<String, String> {
        let config_path = config_path();
        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        let mut overrides = HashMap::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().trim_matches('"');
                let value = value.trim().trim_matches('"');
                overrides.insert(key.to_string(), value.to_string());
            }
        }
        overrides
    }
}

fn config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string());
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());

    PathBuf::from(home)
        .join(".config")
        .join("npx-interceptor.toml")
}
