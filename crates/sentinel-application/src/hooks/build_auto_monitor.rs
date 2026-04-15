//! Build Auto-Monitor
//!
//! PostToolUse hook that detects background builds and injects
//! monitoring suggestions.
//!
//! Detects:
//! - `cargo build --release` run in background → suggest Monitor tool
//! - `npm run build` / `pnpm build` in background → suggest Monitor
//! - `sentinel stage` → remind about staged binary consumption

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process a PostToolUse Bash event for build-related commands.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    // Detect background builds (run_in_background or & suffix)
    let is_background = input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("run_in_background"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || cmd.trim_end().ends_with('&');

    if !is_background {
        // Detect sentinel stage — remind about staged binary
        if cmd.contains("sentinel stage") {
            return HookOutput::inject_context(
                HookEvent::PostToolUse,
                "[Build Monitor] Binary staged. It will auto-swap on next session start. \
                 To apply now, run the new binary directly for regeneration tasks."
                    .to_string(),
            );
        }
        return HookOutput::allow();
    }

    // Background Rust build
    if cmd.contains("cargo build") {
        return HookOutput::inject_context(
            HookEvent::PostToolUse,
            "[Build Monitor] Background `cargo build` detected. \
             The build will notify you when complete via the background task system. \
             Continue working — you'll see a task-notification when it finishes."
                .to_string(),
        );
    }

    // Background Node build
    if cmd.contains("npm run build")
        || cmd.contains("pnpm build")
        || cmd.contains("yarn build")
        || cmd.contains("next build")
    {
        return HookOutput::inject_context(
            HookEvent::PostToolUse,
            "[Build Monitor] Background Node build detected. \
             Continue working — you'll see a task-notification when it finishes."
                .to_string(),
        );
    }

    HookOutput::allow()
}

fn extract_bash_command(input: &HookInput) -> Option<&str> {
    input.tool_input.as_ref()?.get("command")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bg_bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({
                "command": cmd,
                "run_in_background": true
            })),
            ..Default::default()
        }
    }

    fn fg_bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": cmd})),
            ..Default::default()
        }
    }

    #[test]
    fn test_detects_background_cargo_build() {
        let output = process(&bg_bash_input("cargo build --release"));
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("Build Monitor"));
    }

    #[test]
    fn test_detects_background_npm_build() {
        let output = process(&bg_bash_input("npm run build"));
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("Build Monitor"));
    }

    #[test]
    fn test_ignores_foreground_builds() {
        let output = process(&fg_bash_input("cargo build --release"));
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_detects_sentinel_stage() {
        let output = process(&fg_bash_input("sentinel stage"));
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("Binary staged"));
    }

    #[test]
    fn test_ignores_non_build_commands() {
        assert!(process(&bg_bash_input("git status")).hook_specific_output.is_none());
    }

    #[test]
    fn test_ignores_no_input() {
        assert!(process(&HookInput::default()).hook_specific_output.is_none());
    }
}
