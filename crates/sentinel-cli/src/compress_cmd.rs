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
    let Some((program, args)) = cmd.split_first() else {
        bail!("compress: no command provided (usage: sentinel compress -- <cmd> [args…])");
    };

    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("compress: failed to spawn `{program}`: {e}"))?;

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

    let command_str = std::iter::once(program.as_str())
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");

    let bypass = matches!(
        std::env::var(BYPASS_ENV).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes")
    );

    if bypass {
        print!("{raw}");
        eprintln!("[sentinel][compress] bypass on — output passed through verbatim");
        return Ok(exit_code);
    }

    let result = compress(&command_str, &raw);
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
        // `cmd /C exit 3` on Windows; `sh -c 'exit 3'` elsewhere. Use a
        // cross-platform approach via the OS shell.
        #[cfg(windows)]
        let cmd = vec!["cmd".to_string(), "/C".to_string(), "exit 3".to_string()];
        #[cfg(not(windows))]
        let cmd = vec!["sh".to_string(), "-c".to_string(), "exit 3".to_string()];
        let code = run(&cmd).expect("spawn ok");
        assert_eq!(code, 3);
    }

    #[test]
    fn spawn_failure_is_error() {
        let r = run(&["definitely-not-a-real-program-xyz".to_string()]);
        assert!(r.is_err());
    }
}
