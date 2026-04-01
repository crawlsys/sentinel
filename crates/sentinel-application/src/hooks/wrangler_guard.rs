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
static WRANGLER_RS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bwrangler-rs\b").unwrap());
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

    // ── Node wrangler deploy — ALLOW if CWD matches a known service dir ──
    if WRANGLER_DEPLOY.is_match(cmd) {
        let cwd = input.cwd.as_deref().unwrap_or("");
        let normalized = cwd.replace('\\', "/");
        // Known service directories (with and without /c/ prefix, and \\?\ UNC prefix)
        let clean = normalized
            .trim_start_matches("//?/")
            .trim_start_matches("\\\\?\\");
        let suffixes: &[&str] = &[
            "firefly-pro-routing/rust/langgraph-service",
            "firefly-pro-routing/osm-service",
            "firefly-pro-routing/rust/engine-service",
            "firefly-pro-routing",
            "firefly-pro-hyperswitch",
        ];

        // Match if CWD ends with a known suffix (but not a partial match)
        let is_known = suffixes.iter().any(|suffix| {
            clean.ends_with(suffix)
                && (clean.len() == suffix.len()
                    || clean.as_bytes()[clean.len() - suffix.len() - 1] == b'/')
        });

        if is_known {
            return HookOutput::allow();
        }

        return HookOutput::deny(
            "BLOCKED: Node wrangler deploy from unknown directory.\n\n\
             Node wrangler deploy is only allowed from known service directories.\n\
             Either cd to the correct directory or use wrangler-rs.\n\n\
             Allowed directories:\n  \
               API:        firefly-pro-routing/\n  \
               OSM:        firefly-pro-routing/osm-service/\n  \
               AI:         firefly-pro-routing/rust/langgraph-service/\n  \
               Engine:     firefly-pro-routing/rust/engine-service/
  \n               Hyperswitch: firefly-pro-hyperswitch/",
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
///
/// In test mode (`SENTINEL_TEST=1`), always returns false without launching
/// a GUI dialog — prevents tests from hanging on interactive input.
fn show_delete_dialog(command: &str, directory: &str) -> bool {
    // Skip dialog in test/CI environments to prevent hangs
    if std::env::var("SENTINEL_TEST").is_ok() || std::env::var("CI").is_ok() {
        tracing::debug!("Skipping delete dialog in test/CI mode");
        return false;
    }

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

    fn bash_input_with_cwd(command: &str, cwd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(json!({ "command": command })),
            cwd: Some(cwd.to_string()),
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
    fn test_node_wrangler_deploy_blocked_unknown_dir() {
        // No CWD or unknown CWD → blocked
        let output = process(&bash_input("npx wrangler deploy --env staging"));
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_node_wrangler_deploy_allowed_ai_dir() {
        let output = process(&bash_input_with_cwd(
            "npx wrangler deploy --env staging",
            "/c/Users/garys/Documents/GitHub/firefly-pro-routing/rust/langgraph-service",
        ));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_node_wrangler_deploy_allowed_osm_dir() {
        let output = process(&bash_input_with_cwd(
            "npx wrangler deploy --env staging",
            "C:/Users/garys/Documents/GitHub/firefly-pro-routing/osm-service",
        ));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_node_wrangler_deploy_allowed_api_root() {
        let output = process(&bash_input_with_cwd(
            "npx wrangler deploy --env production",
            "/c/Users/garys/Documents/GitHub/firefly-pro-routing",
        ));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_node_wrangler_deploy_allowed_engine_dir() {
        let output = process(&bash_input_with_cwd(
            "npx wrangler deploy --env staging",
            "C:/Users/garys/Documents/GitHub/firefly-pro-routing/rust/engine-service",
        ));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_node_wrangler_deploy_blocked_wrong_dir() {
        let output = process(&bash_input_with_cwd(
            "npx wrangler deploy --env staging",
            "/c/Users/garys/Documents/GitHub/some-other-project",
        ));
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_node_wrangler_deploy_allowed_unc_prefix() {
        let output = process(&bash_input_with_cwd(
            "npx wrangler deploy --env staging",
            "\\\\?\\C:\\Users\\garys\\Documents\\GitHub\\firefly-pro-routing\\rust\\langgraph-service",
        ));
        assert!(output.blocked.is_none());
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
        // Set SENTINEL_TEST to prevent show_delete_dialog from launching the
        // real GUI dialog (which hangs waiting for user input in test env)
        std::env::set_var("SENTINEL_TEST", "1");
        let output = process(&bash_input(
            "wrangler-rs delete --name hook-test-disposable",
        ));
        assert_eq!(output.blocked, Some(true));
        std::env::remove_var("SENTINEL_TEST");
    }

    #[test]
    fn test_no_tool_input_allowed() {
        let output = process(&HookInput::default());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_plain_wrangler_deploy_blocked_no_cwd() {
        let output = process(&bash_input("wrangler deploy --env production"));
        assert_eq!(output.blocked, Some(true));
    }
}
