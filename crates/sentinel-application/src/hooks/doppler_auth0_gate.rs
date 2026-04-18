//! Doppler & Auth0 Mutation Gate
//!
//! HARD BLOCK on all Doppler and Auth0 write/mutation operations.
//! The CLAUDE.md says: "ALWAYS ask for permission before changing anything
//! regarding Doppler or Auth0. NO EXCEPTIONS."
//!
//! Read-only operations (list, get, download) are allowed.
//! All mutations (set, create, delete, update, lock, unlock, clone, rollback)
//! are blocked with a message telling Claude to ask the user first.

use sentinel_domain::events::{HookInput, HookOutput};

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
pub fn process(input: &HookInput) -> HookOutput {
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

        // Block everything else (mutations)
        return HookOutput::deny(format!(
            "🔴 [Doppler/Auth0 Gate] BLOCKED: Doppler mutation `{op}` requires explicit user permission. \
             Ask Gary before making ANY changes to Doppler secrets or configuration. NO EXCEPTIONS."
        ));
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with_tool(tool: &str) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_allows_non_doppler_auth0_tools() {
        assert!(process(&input_with_tool("Edit")).blocked.is_none());
        assert!(process(&input_with_tool("Bash")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__linear__create_issue")).blocked.is_none());
    }

    #[test]
    fn test_blocks_auth0_mutation_tools() {
        assert_eq!(process(&input_with_tool("mcp__auth0__authenticate")).blocked, Some(true));
    }

    #[test]
    fn test_allows_mcp_router_tools_on_all_servers() {
        // mcp-router management tools should always pass through
        assert!(process(&input_with_tool("mcp__auth0__mcp_health_check")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__auth0__mcp_list_servers")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__auth0__mcp_restart_server")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__mcp_health_check")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__mcp_list_servers")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__mcp_restart_server")).blocked.is_none());
    }

    #[test]
    fn test_allows_doppler_read_ops() {
        assert!(process(&input_with_tool("mcp__doppler__get_secret")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__list_projects")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__list_secrets")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__download_secrets")).blocked.is_none());
        assert!(process(&input_with_tool("mcp__doppler__current_account")).blocked.is_none());
    }

    #[test]
    fn test_blocks_doppler_mutations() {
        assert_eq!(process(&input_with_tool("mcp__doppler__set_secret")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__set_secrets")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__delete_secret")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__create_project")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__delete_config")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__lock_config")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__rollback_config")).blocked, Some(true));
        assert_eq!(process(&input_with_tool("mcp__doppler__create_service_token")).blocked, Some(true));
    }

    #[test]
    fn test_allows_no_tool_name() {
        assert!(process(&HookInput::default()).blocked.is_none());
    }
}
