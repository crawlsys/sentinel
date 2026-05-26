//! `output_compressor` — `PreToolUse` hook that auto-routes noisy Bash commands
//! through `sentinel compress` (sentinel's native "RTK").
//!
//! When a `Bash` tool call runs a command whose output is known to be noisy
//! (`cargo test`, `cargo build`/`clippy`, `git status`, `grep`, `find`, …),
//! this hook rewrites the command via [`HookOutput::rewrite_input`] to
//! `sentinel compress -- <original command>`. The CLI runs the real command,
//! structurally compresses the output (preserving every error/result/warning
//! line verbatim — see [`sentinel_domain::output_compress`]), and emits the
//! compressed form, so the agent's captured stdout carries ~70–90% less noise.
//!
//! ## Safety / opt-out
//!
//! - `SENTINEL_COMPRESS_BYPASS` (any of `1`/`true`/`yes`) disables rewriting
//!   entirely — the command runs raw.
//! - Already-wrapped commands (`sentinel compress …`) are left untouched
//!   (no double-wrap).
//! - The domain compressor's signal-preservation invariant guarantees the
//!   verification gate / proof chain still see `test result:`, `error[…]`,
//!   `FAILED`, etc. — so compression can only ever remove noise, never the
//!   lines a downstream gate parses.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::output_compress::{classify, CommandKind};

use super::EnvPort;

/// Env flag that disables compression rewriting.
const BYPASS_ENV: &str = "SENTINEL_COMPRESS_BYPASS";

/// Decide whether `command` should be routed through `sentinel compress`.
/// Pure + testable: no rewrite for non-compressible kinds, already-wrapped
/// commands, or when bypass is set.
#[must_use]
pub fn should_compress(command: &str, bypass: bool) -> bool {
    if bypass {
        return false;
    }
    // Never double-wrap.
    if command.contains("sentinel compress") {
        return false;
    }
    !matches!(classify(command), CommandKind::Other)
}

/// Build the rewritten command that pipes the original through the compressor.
/// The original command is passed verbatim after `--` so its own quoting /
/// chaining (`cd … && cargo test`) is preserved as a single shell string.
#[must_use]
pub fn wrap_command(command: &str) -> String {
    format!("sentinel compress -- {command}")
}

/// Process a `PreToolUse` event. Rewrites a compressible Bash command's input
/// to route through `sentinel compress`; otherwise allows it unchanged.
#[must_use]
pub fn process(input: &HookInput, env: &dyn EnvPort) -> HookOutput {
    if input.tool_name.as_deref() != Some("Bash") {
        return HookOutput::allow();
    }
    let Some(tool_input) = input.tool_input.as_ref() else {
        return HookOutput::allow();
    };
    let command = tool_input
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if command.is_empty() {
        return HookOutput::allow();
    }

    let bypass = matches!(
        env.var(BYPASS_ENV).as_deref(),
        Some("1" | "true" | "TRUE" | "yes")
    );
    if !should_compress(command, bypass) {
        return HookOutput::allow();
    }

    // Rewrite only the `command` field; preserve any other fields the tool
    // input carries (timeout, description, …).
    let mut updated = tool_input.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert(
            "command".to_string(),
            serde_json::Value::String(wrap_command(command)),
        );
    } else {
        return HookOutput::allow();
    }
    HookOutput::rewrite_input(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support;

    #[test]
    fn should_compress_recognizes_noisy_commands() {
        assert!(should_compress("cargo test --workspace", false));
        assert!(should_compress("cd /repo && cargo clippy", false));
        assert!(should_compress("grep -rn foo src", false));
        assert!(!should_compress("echo hello", false));
    }

    #[test]
    fn bypass_disables_compression() {
        assert!(!should_compress("cargo test", true));
    }

    #[test]
    fn never_double_wraps() {
        assert!(!should_compress("sentinel compress -- cargo test", false));
    }

    #[test]
    fn wrap_preserves_original_command_verbatim() {
        assert_eq!(
            wrap_command("cd /repo && cargo test -p foo"),
            "sentinel compress -- cd /repo && cargo test -p foo"
        );
    }

    fn bash_input(command: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({ "command": command, "timeout": 5000 })),
            ..Default::default()
        }
    }

    #[test]
    fn compressible_command_is_rewritten() {
        let env = test_support::StubEnv::new();
        let out = process(&bash_input("cargo test --workspace"), &env);
        let hso = out.hook_specific_output.expect("hookSpecificOutput");
        let updated = hso.updated_input.expect("updated_input");
        assert_eq!(
            updated.get("command").and_then(|c| c.as_str()),
            Some("sentinel compress -- cargo test --workspace")
        );
        // Other fields preserved.
        assert_eq!(updated.get("timeout").and_then(serde_json::Value::as_u64), Some(5000));
    }

    #[test]
    fn non_compressible_command_passes_through() {
        let env = test_support::StubEnv::new();
        let out = process(&bash_input("echo hello"), &env);
        assert!(out.hook_specific_output.is_none());
        assert!(out.blocked.is_none());
    }

    #[test]
    fn bypass_env_passes_through() {
        let env = test_support::StubEnv::with(&[(BYPASS_ENV, "1")]);
        let out = process(&bash_input("cargo test"), &env);
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn non_bash_tool_passes_through() {
        let env = test_support::StubEnv::new();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        let out = process(&input, &env);
        assert!(out.hook_specific_output.is_none());
    }
}
