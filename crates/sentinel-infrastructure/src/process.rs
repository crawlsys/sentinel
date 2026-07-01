//! Real process adapter — implements `ProcessPort`.

use std::io::Read;

use anyhow::Context;
use sentinel_domain::port_errors::ProcessError;
use sentinel_domain::ports::{ProcessOutput, ProcessPort};

/// Poll interval for the `run_with_timeout` `try_wait` loop.
const TRY_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Drain a child pipe to a string on a dedicated thread so a chatty child can
/// never deadlock against a full pipe buffer while the caller polls its exit
/// status.
fn drain_pipe(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = pipe {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).to_string()
    })
}

/// Infrastructure adapter implementing `ProcessPort` via `std::process::Command`.
pub struct RealProcess;

impl ProcessPort for RealProcess {
    fn run(
        &self,
        command: &str,
        args: &[&str],
        cwd: Option<&str>,
    ) -> Result<ProcessOutput, ProcessError> {
        let mut cmd = std::process::Command::new(command);
        cmd.args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let output = cmd
            .output()
            .with_context(|| format!("failed to run: {command}"))
            .map_err(ProcessError::backend)?;

        Ok(ProcessOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    /// Spawn + `try_wait` polling loop with a deadline; kill on expiry.
    ///
    /// Hand-rolled on `std` only — the workspace carries no `wait-timeout`
    /// crate, and dragging an async runtime into a synchronous hook path
    /// would be far heavier than a 25 ms poll.
    ///
    /// On unix the child is started as its own process-group leader (the
    /// safe `CommandExt::process_group(0)`; `pre_exec` is off the table —
    /// this workspace forbids `unsafe`) and the timeout SIGKILLs the WHOLE
    /// group, so descendants spawned by the child cannot outlive the
    /// wall-clock bound. On non-unix only the direct child is killed —
    /// descendants may survive (documented limitation; std has no portable
    /// group/job kill there).
    fn run_with_timeout(
        &self,
        command: &str,
        args: &[&str],
        cwd: Option<&str>,
        timeout: std::time::Duration,
    ) -> Result<ProcessOutput, ProcessError> {
        let mut cmd = std::process::Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // New process group with pgid == child pid, so the timeout path
            // can address the child AND its descendants as one unit.
            cmd.process_group(0);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to run: {command}"))
            .map_err(ProcessError::backend)?;

        let stdout_reader = drain_pipe(child.stdout.take());
        let stderr_reader = drain_pipe(child.stderr.take());

        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        #[cfg(unix)]
                        {
                            // SIGKILL the whole group (uncatchable). The
                            // `kill` binary is used because a raw libc::kill
                            // needs `unsafe`, which the workspace forbids.
                            let pgid = child.id();
                            let _ = std::process::Command::new("kill")
                                .args(["-9", "--", &format!("-{pgid}")])
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .status();
                        }
                        let _ = child.kill();
                        // Reap the direct child.
                        let _ = child.wait();
                        // Deliberately DETACH the drain threads instead of
                        // joining: a descendant that escaped the group (e.g.
                        // via setsid) could hold the pipe open and block a
                        // join far past the caller's deadline. The threads
                        // exit on their own once the pipe closes, and the
                        // timeout path doesn't need the output.
                        drop(stdout_reader);
                        drop(stderr_reader);
                        // `Duration`'s Debug renders the exact configured
                        // value ("200ms", "10s") — `as_secs()` would
                        // truncate sub-second timeouts to "0s" in logs.
                        return Err(ProcessError::Timeout(format!(
                            "{command} exceeded {timeout:?} and was killed"
                        )));
                    }
                    std::thread::sleep(TRY_WAIT_POLL);
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcessError::backend(format!(
                        "failed to wait for {command}: {e}"
                    )));
                }
            }
        };

        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        Ok(ProcessOutput {
            success: status.success(),
            stdout,
            stderr,
        })
    }

    fn spawn_detached(&self, command: &str, args: &[&str]) -> Result<(), ProcessError> {
        std::process::Command::new(command)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn: {command}"))
            .map_err(ProcessError::backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_completes_fast_command() {
        let p = RealProcess;
        let out = p
            .run_with_timeout(
                "sh",
                &["-c", "printf hello; printf world >&2"],
                None,
                std::time::Duration::from_secs(10),
            )
            .unwrap();
        assert!(out.success);
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "world");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_hung_command() {
        let p = RealProcess;
        let start = std::time::Instant::now();
        let err = p
            .run_with_timeout(
                "sh",
                &["-c", "sleep 30"],
                None,
                std::time::Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(
            matches!(err, ProcessError::Timeout(_)),
            "expected Timeout, got: {err}"
        );
        // Deliberately generous bound for loaded shared CI runners: the
        // signal is "killed instead of waited to completion" (the sleep runs
        // 30s), not precise kill latency. The Timeout variant above is the
        // real assertion.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "child must be killed well before its natural 30s duration; took {:?}",
            start.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_returns_promptly_despite_descendants() {
        // The child spawns a background descendant that inherits the stdout
        // pipe and would hold it open for 30s. Before the group-kill +
        // detach fix, killing only the direct child left the orphan alive
        // and the drain-thread join blocked until the orphan exited —
        // busting the wall-clock bound by ~30s.
        let p = RealProcess;
        let start = std::time::Instant::now();
        let err = p
            .run_with_timeout(
                "sh",
                &["-c", "sleep 30 & sleep 30"],
                None,
                std::time::Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(
            matches!(err, ProcessError::Timeout(_)),
            "expected Timeout, got: {err}"
        );
        // Generous bound for loaded CI runners (see the hung-command test):
        // pre-fix this blocked ~30s on the drain-thread join, so anything
        // well under the descendant's 30s lifetime proves the fix.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "timeout must return promptly even when a descendant holds the pipe; took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_missing_binary_is_backend_error() {
        let p = RealProcess;
        let err = p
            .run_with_timeout(
                "definitely-not-a-real-binary-xyz",
                &[],
                None,
                std::time::Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(matches!(err, ProcessError::Backend(_)), "got: {err}");
    }
}
