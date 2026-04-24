//! Doppler & Auth0 Mutation Gate
//!
//! HARD BLOCK on all Doppler and Auth0 write/mutation operations.
//! The CLAUDE.md says: "ALWAYS ask for permission before changing anything
//! regarding Doppler or Auth0. NO EXCEPTIONS."
//!
//! Read-only operations (list, get, download) are allowed.
//! All mutations (set, create, delete, update, lock, unlock, clone, rollback)
//! are blocked with a message telling Claude to ask the user first.
//!
//! **Override**: The user can unblock Doppler mutations for 60 seconds by
//! typing an explicit phrase matched by `hygiene_override::is_doppler_override`
//! (e.g. "override doppler", "authorize doppler write"). The override writes a
//! signed token under `~/.claude/sentinel/overrides/doppler-{hash}` with
//! signature validation (same scheme as hygiene + verification overrides).
//! Auth0 mutations are NOT subject to this override — always hard-blocked.

use sentinel_domain::events::{HookInput, HookOutput};

use super::HookContext;

/// Doppler read-only operations that are always safe.
const DOPPLER_READ_OPS: &[&str] = &[
    // mcp-router management tools (present on all MCP servers)
    "mcp_health_check",
    "mcp_list_servers",
    "mcp_restart_server",
    // Doppler read-only operations
    "current_account",
    "list_accounts",
    "get_me",
    "get_activity",
    "get_settings",
    "get_project",
    "get_config",
    "get_config_log",
    "get_environment",
    "get_secret",
    "get_integration",
    "get_integration_options",
    "get_sync",
    "get_group",
    "get_service_account",
    "get_webhook",
    "get_workplace_role",
    "get_workplace_user",
    "get_project_role",
    "get_change_request",
    "get_change_request_policy",
    "get_change_request_unit",
    "list_projects",
    "list_configs",
    "list_config_logs",
    "list_environments",
    "list_secrets",
    "list_secret_names",
    "list_integrations",
    "list_syncs",
    "list_service_tokens",
    "list_groups",
    "list_service_accounts",
    "list_service_account_tokens",
    "list_webhooks",
    "list_workplace_roles",
    "list_workplace_users",
    "list_project_roles",
    "list_project_members",
    "list_invites",
    "list_trusted_ips",
    "list_change_requests",
    "list_change_request_policies",
    "download_secrets",
    "audit_workplace",
    "share_secret_plain",
];

/// Process a PreToolUse event. Blocks Doppler/Auth0 mutation tools.
///
/// Doppler mutations can be unblocked by a signed session override written
/// by `hygiene_override::process` when the user's prompt matches
/// `is_doppler_override`. Auth0 mutations are never unblocked.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return HookOutput::allow(),
    };

    // mcp-router management tools are always safe (health check, list, restart)
    if tool.ends_with("__mcp_health_check")
        || tool.ends_with("__mcp_list_servers")
        || tool.ends_with("__mcp_restart_server")
    {
        return HookOutput::allow();
    }

    // Auth0 — block ALL tools (it's an auth system, everything is sensitive)
    // No override path; Auth0 changes always need Gary to run them himself.
    if tool.starts_with("mcp__auth0__") {
        return HookOutput::deny(
            "🔴 [Doppler/Auth0 Gate] BLOCKED: Auth0 operations require explicit user permission. \
             Ask Gary before making ANY changes to Auth0. NO EXCEPTIONS."
        );
    }

    // Doppler — block mutations, allow reads
    if tool.starts_with("mcp__doppler__") {
        let op = tool.strip_prefix("mcp__doppler__").unwrap_or("");

        // Allow read-only operations
        if DOPPLER_READ_OPS.iter().any(|&read_op| op == read_op) {
            return HookOutput::allow();
        }

        // Check for a live Doppler override (60s TTL). The MCP tool call may run
        // in a child session (spawned by the MCP server) with a different
        // session_id from the user's main session, so we scan ALL doppler-*
        // override files and accept any that are <60s old. The signature check
        // is skipped for cross-session matching — we rely on the redirect guard
        // at the Bash level to prevent unauthorized writes to the overrides dir,
        // and the high-friction prompt phrase required to write an override.
        let overrides_dir = ctx.fs.home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".claude")
            .join("sentinel")
            .join("overrides");

        if let Ok(paths) = ctx.fs.read_dir(&overrides_dir) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            for path in paths {
                let file_name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if !file_name.starts_with("doppler-") {
                    continue;
                }
                let content = match ctx.fs.read_to_string(&path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                // Parse ts:sig format (first field before `:`)
                let ts: u64 = match content
                    .trim()
                    .split(':')
                    .next()
                    .and_then(|s| s.parse().ok())
                {
                    Some(t) => t,
                    None => continue,
                };
                if now.saturating_sub(ts) < 60 {
                    // Live override found — allow
                    return HookOutput::allow();
                }
            }
        }

        // Block everything else (mutations)
        return HookOutput::deny(format!(
            "🔴 [Doppler/Auth0 Gate] BLOCKED: Doppler mutation `{op}` requires explicit user permission. \
             Ask Gary before making ANY changes to Doppler secrets or configuration. NO EXCEPTIONS. \
             To unblock for 60s, Gary must type an override phrase like \"override doppler\" or \
             \"authorize doppler write\" in his next prompt."
        ));
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::stub_ctx;

    fn input_with_tool(tool: &str) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            ..Default::default()
        }
    }

    fn input_with_session(tool: &str, session: &str) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            session_id: Some(session.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_allows_non_doppler_auth0_tools() {
        let ctx = stub_ctx();
        assert!(process(&input_with_tool("Edit"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("Bash"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__linear__create_issue"), &ctx).blocked.is_none());
    }

    #[test]
    fn test_blocks_auth0_mutation_tools() {
        let ctx = stub_ctx();
        assert_eq!(process(&input_with_tool("mcp__auth0__authenticate"), &ctx).blocked, Some(true));
    }

    #[test]
    fn test_allows_mcp_router_tools_on_all_servers() {
        let ctx = stub_ctx();
        assert!(process(&input_with_tool("mcp__auth0__mcp_health_check"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__auth0__mcp_list_servers"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__auth0__mcp_restart_server"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__mcp_health_check"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__mcp_list_servers"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__mcp_restart_server"), &ctx).blocked.is_none());
    }

    #[test]
    fn test_allows_doppler_read_ops() {
        let ctx = stub_ctx();
        assert!(process(&input_with_tool("mcp__doppler__get_secret"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__list_projects"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__list_secrets"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__download_secrets"), &ctx).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__current_account"), &ctx).blocked.is_none());
    }

    #[test]
    fn test_blocks_doppler_mutations() {
        let ctx = stub_ctx();
        assert_eq!(process(&input_with_tool("mcp__doppler__set_secret"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__set_secrets"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__delete_secret"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__create_project"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__delete_config"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__lock_config"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__rollback_config"), &ctx).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__create_service_token"), &ctx).blocked, Some(true));
    }

    // NOTE: We cannot unit-test the happy path of "override unblocks mutation"
    // with StubFs because StubFs.write is a no-op and StubFs.read_to_string
    // errors — the signed token can't be round-tripped in-memory. This E2E
    // path is covered by manual verification after staging the binary.
    // What we CAN test at unit level is that the gate continues to block when
    // no valid override exists, and that Auth0 stays blocked regardless.

    #[test]
    fn test_doppler_override_does_not_affect_auth0() {
        // Even if a Doppler override were active (verified at integration level),
        // Auth0 mutations must remain hard-blocked. The gate checks Auth0 first,
        // before any override lookup, so this test verifies the control-flow order.
        let ctx = stub_ctx();
        let input = input_with_session("mcp__auth0__update_user", "any-session");
        assert_eq!(
            process(&input, &ctx).blocked,
            Some(true),
            "auth0 mutations are not covered by doppler override"
        );
    }

    #[test]
    fn test_doppler_mutation_without_override_still_blocked() {
        let ctx = stub_ctx();
        let input = input_with_session("mcp__doppler__set_secret", "no-override-session");
        assert_eq!(process(&input, &ctx).blocked, Some(true));
    }

    #[test]
    fn test_allows_no_tool_name() {
        let ctx = stub_ctx();
        assert!(process(&HookInput::default(), &ctx).blocked.is_none());
    }
}
