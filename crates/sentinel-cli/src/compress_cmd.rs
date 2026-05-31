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

use std::io::Write;
use std::process::Command;

use anyhow::{bail, Result};
use sentinel_domain::output_compress::{classify, compress, CommandKind};

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
///
/// ## Correctness guarantees (the corruption fixes)
///
/// 1. **Real stream order.** stderr is merged into stdout *at the OS level*
///    (`2>&1` at spawn time) so the captured bytes are in the true interleaved
///    order the terminal would show. The previous impl captured the two
///    streams separately and concatenated all-stdout-then-all-stderr, which
///    relocated interleaved diagnostics to the bottom — a reordered transcript.
/// 2. **Byte-exact passthrough.** When the command is not compressible
///    (`CommandKind::Other`) OR its output is not valid UTF-8, the raw bytes are
///    written through **verbatim** — no `from_utf8_lossy`, so non-UTF-8 output
///    (binary dumps, latin-1, hexdumps) is never mangled into U+FFFD. The lossy
///    conversion now happens *only* when a compression rule will actually run on
///    valid-UTF-8 text.
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

    // **Fix #1 (ordering):** merge stderr into stdout *inside the shell
    // invocation* (`2>&1`), so the kernel interleaves the two streams in real
    // time — exactly as a terminal shows them. We then capture the single
    // stdout pipe. The previous impl captured two separate pipes and
    // concatenated all-stdout-then-all-stderr, relocating interleaved
    // diagnostics to the bottom. See `shell_command`.
    let output = shell_command(&command_line)
        .output()
        .map_err(|e| anyhow::anyhow!("compress: failed to spawn shell for `{command_line}`: {e}"))?;

    let exit_code = output.status.code().unwrap_or(1);
    let raw_bytes = output.stdout; // merged stdout+stderr in true order (2>&1)

    let command_str = command_line.as_str();

    let bypass = matches!(
        std::env::var(BYPASS_ENV).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes")
    );

    let rendered = render_output(command_str, &raw_bytes, bypass);
    write_stdout_bytes(&rendered.bytes)?;
    if let Some(footer) = rendered.footer {
        eprintln!("{footer}");
    }
    Ok(exit_code)
}

/// What `render_output` decided to emit: the exact stdout bytes plus an
/// optional human-facing stderr footer.
struct Rendered {
    /// Bytes written verbatim to stdout (the agent's captured output).
    bytes: Vec<u8>,
    /// One-line summary for a human tailing stderr; `None` = no footer.
    footer: Option<String>,
}

/// Decide what to emit for `raw` produced by `command_str`. Pure + testable —
/// no IO. Encodes both corruption fixes:
///
/// - **Byte-exact passthrough:** a bypass, a non-compressible command
///   (`CommandKind::Other`), or non-UTF-8 output returns `raw` **verbatim** —
///   never round-tripped through `from_utf8_lossy`, so binary/latin-1/hexdump
///   bytes survive intact.
/// - **Compression** only runs on a compressible command whose output is valid
///   UTF-8; signal lines are preserved by the domain rules.
fn render_output(command_str: &str, raw: &[u8], bypass: bool) -> Rendered {
    if bypass {
        return Rendered {
            bytes: raw.to_vec(),
            footer: Some("[sentinel][compress] bypass on — output passed through verbatim".into()),
        };
    }

    if matches!(classify(command_str), CommandKind::Other) {
        // Ordinary command — verbatim, no footer (avoids noise on every shell call).
        return Rendered { bytes: raw.to_vec(), footer: None };
    }

    let Ok(text) = std::str::from_utf8(raw) else {
        // Compressible by command, but non-UTF-8 bytes — never mangle them.
        return Rendered {
            bytes: raw.to_vec(),
            footer: Some(
                "[sentinel][compress] non-UTF-8 output — passed through verbatim (byte-exact)".into(),
            ),
        };
    };

    let result = compress(command_str, text);
    let mut bytes = result.compressed.clone().into_bytes();
    if !result.compressed.ends_with('\n') && !result.compressed.is_empty() {
        bytes.push(b'\n');
    }
    let footer = format!(
        "[sentinel][compress] {} → {} bytes ({:.0}% saved, {} line(s) dropped) for `{}`",
        result.original_bytes,
        result.compressed_bytes,
        result.savings_ratio() * 100.0,
        result.lines_dropped,
        command_str,
    );
    Rendered { bytes, footer: Some(footer) }
}

/// Write raw bytes to stdout verbatim (no UTF-8 round-trip), flushing after.
fn write_stdout_bytes(bytes: &[u8]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(bytes)?;
    lock.flush()?;
    Ok(())
}


/// Build a `Command` that runs `line` through the platform shell, so shell
/// builtins, operators, pipes, redirects, env-prefixes and globs all work —
/// matching how the Bash tool that produced the wrapped command runs it.
///
/// stderr is merged into stdout *inside the shell* so the kernel interleaves
/// both streams in real order (the ordering-corruption fix). On POSIX shells
/// we wrap as `{ <line> ; } 2>&1`; on `cmd.exe` as `( <line> ) 2>&1`.
fn shell_command(line: &str) -> Command {
    if cfg!(windows) {
        // Prefer bash if present (the harness's Bash tool uses Git bash), so
        // POSIX command lines (`cd x && cargo …`, `export FOO=…`) behave
        // identically. Fall back to cmd.exe when bash isn't on PATH.
        if let Ok(bash) = which_bash() {
            let mut c = Command::new(bash);
            c.arg("-c").arg(merge_posix(line));
            c
        } else {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(format!("( {line} ) 2>&1"));
            c
        }
    } else {
        // Use an absolute path to the shell rather than relying on PATH lookup.
        // A wrapped command can legitimately mutate PATH (`PATH=… cmd`), and we
        // must still be able to spawn the shell; resolving `sh` via PATH would
        // break in that window. `/bin/sh` is the POSIX-guaranteed location.
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(merge_posix(line));
        c
    }
}

/// Wrap a POSIX shell line so stderr merges into stdout in real interleaved
/// order. The trailing newline before `}` lets the line end in a comment or
/// an unterminated construct without swallowing the `2>&1`.
fn merge_posix(line: &str) -> String {
    format!("{{ {line}\n}} 2>&1")
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

    // ── Corruption-fix regression tests ──────────────────────────────────

    /// Fix #2: a non-compressible (`Other`) command's output is emitted
    /// byte-for-byte, even when it contains invalid UTF-8. The old path ran
    /// `from_utf8_lossy` unconditionally, turning these bytes into U+FFFD.
    #[test]
    fn non_utf8_passthrough_is_byte_exact() {
        // `cat` classifies as Other → passthrough. Bytes include a lone 0xFF,
        // a 0xFE, and a truncated multibyte 0xC3 0x28 — all invalid UTF-8.
        let raw = b"\xff\xfe\x00binary\xc3\x28data\n";
        let r = render_output("cat /tmp/x.bin", raw, false);
        assert_eq!(r.bytes, raw, "passthrough must be byte-identical");
        assert!(r.footer.is_none(), "Other command emits no footer");
    }

    /// Fix #2: a *compressible* command that nonetheless emits non-UTF-8 bytes
    /// is also passed through verbatim rather than mangled.
    #[test]
    fn non_utf8_compressible_command_is_byte_exact() {
        let raw = b"\xff\xfenot valid utf8\xc3\x28\n";
        // `grep` is compressible, but the bytes aren't UTF-8 → verbatim.
        let r = render_output("grep -rn foo .", raw, false);
        assert_eq!(r.bytes, raw, "non-UTF-8 must survive even for grep");
        assert!(r
            .footer
            .as_deref()
            .is_some_and(|f| f.contains("non-UTF-8")));
    }

    /// Bypass returns raw bytes verbatim regardless of classification.
    #[test]
    fn bypass_is_byte_exact() {
        let raw = b"cargo noise\n\xff\xfe\n";
        let r = render_output("cargo test", raw, true);
        assert_eq!(r.bytes, raw);
        assert!(r.footer.as_deref().is_some_and(|f| f.contains("bypass on")));
    }

    /// A compressible command with valid UTF-8 still compresses (the feature
    /// keeps working) and preserves signal lines.
    #[test]
    fn compressible_utf8_still_compresses_and_keeps_signal() {
        let raw = "\
   Compiling foo v0.1.0
test a ... ok
test b ... ok
test result: ok. 2 passed; 0 failed; 0 ignored
"
        .as_bytes();
        let r = render_output("cargo test", raw, false);
        let out = String::from_utf8(r.bytes).unwrap();
        assert!(out.contains("test result: ok. 2 passed; 0 failed; 0 ignored"));
        assert!(!out.contains("Compiling foo"), "noise should be dropped");
        assert!(out.contains("passing test(s) (ok) collapsed"));
    }

    /// Fix #1: stderr and stdout are interleaved in true order. We run a
    /// command that alternates the two streams and assert the merged output
    /// keeps source order (OUT-A, ERR-B, OUT-C, ERR-D) — not all-stdout then
    /// all-stderr. Run through a real shell via `run` would write to the
    /// process stdout, so instead we invoke the shell directly the same way
    /// `shell_command` does and inspect the merged bytes.
    #[cfg(not(windows))]
    #[test]
    fn stderr_stdout_interleave_in_true_order() {
        let line = "echo OUT-A; echo ERR-B >&2; echo OUT-C; echo ERR-D >&2";
        let out = shell_command(line).output().expect("spawn");
        let merged = String::from_utf8_lossy(&out.stdout);
        // stderr must be empty — everything merged into stdout via 2>&1.
        assert!(
            out.stderr.is_empty(),
            "stderr should be empty (merged); got {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pos = |s: &str| merged.find(s).unwrap_or(usize::MAX);
        assert!(
            pos("OUT-A") < pos("ERR-B")
                && pos("ERR-B") < pos("OUT-C")
                && pos("OUT-C") < pos("ERR-D"),
            "streams not in source order:\n{merged}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn merge_posix_wraps_with_redirect() {
        assert_eq!(merge_posix("cargo test"), "{ cargo test\n} 2>&1");
    }
}
