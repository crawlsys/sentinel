//! `sentinel compress -- <cmd>` — run a command and emit token-compressed
//! output (sentinel's native "RTK").
//!
//! Runs the wrapped command to completion, capturing stdout+stderr, applies
//! the deterministic [`output_compress`](sentinel_domain::output_compress)
//! rules, prints the compressed text, and exits with the wrapped command's
//! exit code. Set `SENTINEL_COMPRESS_BYPASS=1` to pass output through
//! verbatim (the escape hatch for verification-critical commands).
//!
//! The compressed text is emitted to **stdout** so the calling AI agent's
//! captured output is the compressed form. A one-line savings summary is
//! emitted to **stderr** (visible to a human tailing logs, not part of the
//! agent's captured stdout).

use std::process::Command;

use anyhow::{bail, Result};
use sentinel_domain::output_compress::compress;

/// Environment flag that disables compression (verbatim passthrough).
const BYPASS_ENV: &str = "SENTINEL_COMPRESS_BYPASS";

/// Run `cmd` (argv vector; `cmd[0]` is the program), compress its combined
/// output, print it, and return the process exit code to propagate.
///
/// # Errors
/// Returns an error only when the wrapped command cannot be spawned at all
/// (e.g. program not found). A command that runs and exits non-zero is NOT an
/// error here — its output is compressed and its exit code is returned via
/// `exit_code` for the caller to propagate.
pub fn run(cmd: &[String]) -> Result<i32> {
    if cmd.is_empty() {
        bail!("compress: no command provided (usage: sentinel compress -- <cmd> [args…])");
    }

    // The wrapped command is a SHELL command line, not an argv vector — it can
    // contain shell builtins (`cd`), operators (`&&`, `|`, `>`), env-var
    // prefixes (`FOO=bar cmd`), and globs. Exec'ing `cmd[0]` directly breaks on
    // all of those (the classic symptom: `failed to spawn 'cd'`). Re-join the
    // args into one line and run it through the platform shell, exactly as the
    // Bash tool that produced this command would.
    let command_line = cmd.join(" ");
    let output = shell_command(&command_line)
        .output()
        .map_err(|e| anyhow::anyhow!("compress: failed to spawn shell for `{command_line}`: {e}"))?;

    let exit_code = output.status.code().unwrap_or(1);

    // Combine stdout + stderr the way a shell would interleave them for the
    // agent's view. Tools split error detail across both streams (rustc errors
    // go to stderr), so we must compress the union to preserve signal lines.
    let mut raw = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !raw.is_empty() && !raw.ends_with('\n') {
            raw.push('\n');
        }
        raw.push_str(&stderr);
    }

    let command_str = command_line.as_str();

    let bypass = matches!(
        std::env::var(BYPASS_ENV).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes")
    );

    if bypass {
        print!("{raw}");
        eprintln!("[sentinel][compress] bypass on — output passed through verbatim");
        return Ok(exit_code);
    }

    let result = compress(command_str, &raw);
    print!("{}", result.compressed);
    if !result.compressed.ends_with('\n') && !result.compressed.is_empty() {
        println!();
    }
    eprintln!(
        "[sentinel][compress] {} → {} bytes ({:.0}% saved, {} line(s) dropped) for `{}`",
        result.original_bytes,
        result.compressed_bytes,
        result.savings_ratio() * 100.0,
        result.lines_dropped,
        command_str,
    );
    Ok(exit_code)
}

/// Build a `Command` that runs `line` through the platform shell, so shell
/// builtins, operators, pipes, redirects, env-prefixes and globs all work —
/// matching how the Bash tool that produced the wrapped command runs it.
fn shell_command(line: &str) -> Command {
    if cfg!(windows) {
        // Prefer bash if present (the harness's Bash tool uses Git bash), so
        // POSIX command lines (`cd x && cargo …`, `export FOO=…`) behave
        // identically. Fall back to cmd.exe when bash isn't on PATH.
        if let Ok(bash) = which_bash() {
            let mut c = Command::new(bash);
            c.arg("-c").arg(line);
            c
        } else {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(line);
            c
        }
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(line);
        c
    }
}

/// Locate a `bash` executable on Windows (Git bash). Returns the path string
/// or an error if none is found, so the caller can fall back to cmd.exe.
#[cfg(windows)]
fn which_bash() -> Result<String> {
    // Common Git-for-Windows locations + PATH lookup.
    for candidate in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ] {
        if std::path::Path::new(candidate).exists() {
            return Ok(candidate.to_string());
        }
    }
    // PATH lookup via `where`.
    let out = Command::new("where").arg("bash").output();
    if let Ok(o) = out {
        if o.status.success() {
            if let Some(first) = String::from_utf8_lossy(&o.stdout).lines().next() {
                let p = first.trim();
                if !p.is_empty() {
                    return Ok(p.to_string());
                }
            }
        }
    }
    bail!("bash not found on PATH")
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn which_bash() -> Result<String> {
    bail!("not windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cmd_errors() {
        let r = run(&[]);
        assert!(r.is_err());
    }

    #[test]
    fn propagates_exit_code_of_wrapped_command() {
        // The command is now run through the platform shell, so a bare
        // `exit N` line propagates its code.
        let cmd = vec!["exit".to_string(), "3".to_string()];
        let code = run(&cmd).expect("shell spawn ok");
        assert_eq!(code, 3);
    }

    #[test]
    fn shell_builtin_cd_does_not_break() {
        // Regression: the old impl exec'd `cmd[0]` directly, so a `cd … && …`
        // line failed with "failed to spawn 'cd'". Through a shell it works.
        #[cfg(windows)]
        let cmd = vec!["cd".to_string(), ".".to_string(), "&&".to_string(), "echo".to_string(), "ok".to_string()];
        #[cfg(not(windows))]
        let cmd = vec!["cd".to_string(), ".".to_string(), "&&".to_string(), "echo".to_string(), "ok".to_string()];
        // Must NOT error (the whole point of the fix). Exit code is the
        // command's own; we only assert it ran without a spawn error.
        let r = run(&cmd);
        assert!(r.is_ok(), "cd && echo must run through a shell, got {r:?}");
    }

    #[test]
    fn nonexistent_program_returns_nonzero_not_spawn_error() {
        // Through a shell, an unknown program is a non-zero exit (shell's
        // "command not found"), not a Rust spawn error.
        let r = run(&["definitely-not-a-real-program-xyz".to_string()]);
        assert!(r.is_ok(), "shell spawns fine; the inner command fails");
        assert_ne!(r.unwrap(), 0, "unknown command exits non-zero");
    }
}
