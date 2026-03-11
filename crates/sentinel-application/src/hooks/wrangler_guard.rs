//! Wrangler Guard — Block Node wrangler, enforce wrangler-rs, gate ALL deletes
//!
//! Migrated from `~/.claude/scripts/wrangler-guard.sh`.
//!
//! Rules:
//!   1. wrangler-rs deploy → allow (always safe)
//!   2. Node wrangler deploy → deny (use wrangler-rs instead)
//!   3. ANY wrangler delete (except containers delete) → deny (GUI dialog approval)
//!   4. Node wrangler containers push/list/delete, secret put → allow

use regex::Regex;
use std::sync::LazyLock;

use sentinel_domain::events::{HookInput, HookOutput};

static WRANGLER_DELETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(wrangler-rs|wrangler).*(delete)").unwrap());
static CONTAINERS_DELETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"wrangler.*containers\s+delete").unwrap());
static WRANGLER_RS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bwrangler-rs\b").unwrap());
static NODE_WRANGLER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(npm/wrangler|npx wrangler|wrangler )").unwrap());
static WRANGLER_ALLOWED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"wrangler.*(containers (push|list|delete)|secret put)").unwrap());
static WRANGLER_DEPLOY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"wrangler.*deploy").unwrap());

/// Process a PreToolUse Bash event for wrangler commands.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    // ── DELETE GATE (all wrangler variants) ──────────────────────────────
    if WRANGLER_DELETE.is_match(cmd) {
        // Exception: "wrangler containers delete" — container app cleanup, not worker deletion
        if CONTAINERS_DELETE.is_match(cmd) {
            return HookOutput::allow();
        }

        // Launch native Fluent dialog for approval
        let cwd = input.cwd.as_deref().unwrap_or(".");
        if show_delete_dialog(cmd, cwd) {
            return HookOutput::allow();
        }

        return HookOutput::deny(
            "DENIED: Worker delete was not approved.\n\n\
             Gary declined the delete operation (or the dialog failed to launch).\n\
             No workers were deleted.",
        );
    }

    // ── wrangler-rs (non-delete) — always safe ──────────────────────────
    if WRANGLER_RS.is_match(cmd) {
        return HookOutput::allow();
    }

    // ── Not a wrangler command at all — allow ───────────────────────────
    if !NODE_WRANGLER.is_match(cmd) {
        return HookOutput::allow();
    }

    // ── Node wrangler allowed ops ───────────────────────────────────────
    if WRANGLER_ALLOWED.is_match(cmd) {
        return HookOutput::allow();
    }

    // ── Node wrangler deploy — BLOCK ────────────────────────────────────
    if WRANGLER_DEPLOY.is_match(cmd) {
        return HookOutput::deny(
            "BLOCKED: Node wrangler deploy detected.\n\n\
             USE wrangler-rs INSTEAD: wrangler-rs workers deploy --env <env>\n\
             Node wrangler uses CWD to find wrangler.toml — wrong CWD = wrong service.\n\n\
             Service directories (CWD for wrangler.toml):\n  \
               API:        /c/Users/garys/Documents/GitHub/firefly-pro-routing\n  \
               OSM:        /c/Users/garys/Documents/GitHub/firefly-pro-routing/osm-service\n  \
               AI:         /c/Users/garys/Documents/GitHub/firefly-pro-routing/rust/langgraph-service\n  \
               Engine:     /c/Users/garys/Documents/GitHub/firefly-pro-routing/rust/engine-service\n  \
               Engine AI:  /c/Users/garys/Documents/GitHub/firefly-pro-routing/rust/engine-service",
        );
    }

    // Allow everything else
    HookOutput::allow()
}

fn extract_bash_command(input: &HookInput) -> Option<&str> {
    input.tool_input.as_ref()?.get("command")?.as_str()
}

/// Launch the native Fluent dialog for delete approval.
/// Returns true if approved, false if denied or dialog unavailable.
fn show_delete_dialog(command: &str, directory: &str) -> bool {
    let dialog_path = dirs::home_dir()
        .map(|h| h.join(".claude/scripts/wrangler-guard-dialog.exe"))
        .unwrap_or_default();

    if !dialog_path.exists() {
        tracing::warn!(
            "Wrangler guard dialog not found at {}",
            dialog_path.display()
        );
        return false; // Deny by default when dialog missing
    }

    match std::process::Command::new(&dialog_path)
        .args(["--command", command, "--directory", directory])
        .output()
    {
        Ok(output) => output.status.success(),
        Err(e) => {
            tracing::warn!("Failed to launch wrangler guard dialog: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash_input(command: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(json!({ "command": command })),
            ..Default::default()
        }
    }

    #[test]
    fn test_non_wrangler_allowed() {
        let output = process(&bash_input("ls -la"));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_wrangler_rs_deploy_allowed() {
        let output = process(&bash_input("wrangler-rs workers deploy --env production"));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_node_wrangler_deploy_blocked() {
        let output = process(&bash_input("npx wrangler deploy --env staging"));
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_node_wrangler_containers_push_allowed() {
        let output = process(&bash_input(
            "\"/c/Users/garys/AppData/Roaming/npm/wrangler\" containers push firefly-api:v1.0",
        ));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_node_wrangler_secret_put_allowed() {
        let output = process(&bash_input(
            "\"/c/Users/garys/AppData/Roaming/npm/wrangler\" secret put DATABASE_URL --env dev",
        ));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_containers_delete_allowed() {
        let output = process(&bash_input("npx wrangler containers delete old-app"));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_wrangler_rs_delete_blocked_without_dialog() {
        // Dialog binary won't exist in test environment → deny by default
        let output = process(&bash_input(
            "wrangler-rs delete --name hook-test-disposable",
        ));
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_no_tool_input_allowed() {
        let output = process(&HookInput::default());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_plain_wrangler_deploy_blocked() {
        let output = process(&bash_input("wrangler deploy --env production"));
        assert_eq!(output.blocked, Some(true));
    }
}
