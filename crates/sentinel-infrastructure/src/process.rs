//! Real process adapter — implements `ProcessPort`.

use anyhow::Context;
use sentinel_domain::port_errors::ProcessError;
use sentinel_domain::ports::{ProcessOutput, ProcessPort};

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
