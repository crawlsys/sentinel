//! Doppler & Auth0 Mutation Gate
//!
//! Default (Planned mode): HARD BLOCK on Doppler and Auth0 write/mutation
//! operations. The CLAUDE.md rule is "ALWAYS ask for permission before
//! changing anything regarding Doppler or Auth0" — but only production
//! configs / tenants are the real concern.
//!
//! Read-only operations (list, get, download) are always allowed.
//! All mutations (set, create, delete, update, lock, unlock, clone, rollback)
//! are blocked with a message telling Claude to ask the user first — unless
//! one of the bypasses below applies.
//!
//! **Autopilot authorization**: when `SENTINEL_AUTOPILOT=1`, non-prod Doppler and
//! Auth0 mutations are allowed without asking. A prod config (`prd`, `prod`,
//! `production` — case-insensitive substring in the tool's `config` / `project`
//! / `domain` argument) is still hard-blocked even in Autopilot. When the
//! arguments don't name a config at all, Autopilot is conservative and blocks
//! unless a valid session-scoped signed Doppler override applies.
//!
//! **Explicit override**: The user can unblock Doppler mutations for 5 minutes
//! by typing a phrase matched by `hygiene_override::is_doppler_override`
//! (e.g. "override doppler", "authorize doppler write"). The override writes a
//! signed token under `~/.claude/sentinel/overrides/doppler-{hash}` with
//! signature validation (same scheme as hygiene + verification overrides).
//! Auth0 mutations are NOT subject to this override.

use sentinel_domain::events::{HookInput, HookOutput};

use super::{hygiene_override, EnvPort, HookContext};

/// True iff `SENTINEL_AUTOPILOT` is set to `1` or `true` (case-insensitive).
fn is_autopilot(env: &dyn EnvPort) -> bool {
    env.var("SENTINEL_AUTOPILOT")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Case-insensitive scan for prod markers inside a tool-input value. Returns
/// true if any top-level string field named `config`, `project`, `domain`,
/// `tenant`, or `name` contains the substring `prd`, `prod`, or `production`.
///
/// Absence is production-risk evidence: when the input is `None`, there are no
/// concrete args to prove non-prod scope, so the gate returns `true`. In
/// Autopilot this forces the explicit override path rather than a silent allow.
fn targets_production(tool_input: Option<&serde_json::Value>) -> bool {
    let Some(v) = tool_input else { return true };
    const FIELDS: &[&str] = &["config", "project", "domain", "tenant", "name"];
    const PROD_MARKERS: &[&str] = &["prd", "prod", "production"];
    for field in FIELDS {
        let Some(s) = v.get(*field).and_then(|x| x.as_str()) else {
            continue;
        };
        let lower = s.to_ascii_lowercase();
        if PROD_MARKERS.iter().any(|m| lower.contains(m)) {
            return true;
        }
    }
    // No prod marker found in any relevant field — treat as non-prod.
    false
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DopplerAuth0Provider {
    None,
    Doppler,
    Auth0,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DopplerAuth0Decision {
    Allow,
    AllowReadOnly,
    AllowAutopilotNonProd,
    AllowSignedOverride,
    Block,
}

#[derive(Debug, Clone)]
pub struct DopplerAuth0Evaluation {
    pub tool: Option<String>,
    pub operation: Option<String>,
    pub provider: DopplerAuth0Provider,
    pub router_management: bool,
    pub read_only: bool,
    pub mutation: bool,
    pub autopilot: bool,
    pub tool_input_present: bool,
    pub production_target: bool,
    pub session_id_present: bool,
    pub signed_override_active: bool,
    pub auth0_override_supported: bool,
    pub should_block: bool,
    pub decision: DopplerAuth0Decision,
    pub block_reason: Option<String>,
}

impl DopplerAuth0Evaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        !matches!(self.provider, DopplerAuth0Provider::None)
    }
}

/// Process a `PreToolUse` event. Blocks Doppler/Auth0 mutation tools.
///
/// Doppler mutations can be unblocked by a signed session override written
/// by `hygiene_override::process` when the user's prompt matches
/// `is_doppler_override`. Auth0 mutations are never unblocked.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let evaluation = evaluate(input, ctx);
    output_from_evaluation(&evaluation)
}

pub fn evaluate(input: &HookInput, ctx: &HookContext<'_>) -> DopplerAuth0Evaluation {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return base_evaluation(None, None, DopplerAuth0Provider::None, ctx),
    };
    let provider = provider_for_tool(tool);
    let operation = operation_for_tool(tool, provider);
    let mut evaluation = base_evaluation(
        Some(tool.to_string()),
        operation.map(str::to_string),
        provider,
        ctx,
    );
    evaluation.tool_input_present = input.tool_input.is_some();
    evaluation.session_id_present = input
        .session_id
        .as_deref()
        .is_some_and(|session_id| !session_id.is_empty());
    if evaluation.graph_authority_required() {
        evaluation.production_target = targets_production(input.tool_input.as_ref());
    }

    // mcp-router management tools are always safe (health check, list, restart)
    if tool.ends_with("__mcp_health_check")
        || tool.ends_with("__mcp_list_servers")
        || tool.ends_with("__mcp_restart_server")
    {
        evaluation.router_management = true;
        evaluation.read_only = true;
        evaluation.decision = DopplerAuth0Decision::AllowReadOnly;
        return evaluation;
    }

    // Auth0 — hard-block against production tenants. In Autopilot, non-prod
    // tenants are allowed (the Autopilot directive in CLAUDE.md explicitly
    // permits non-prod Auth0 changes).
    if matches!(provider, DopplerAuth0Provider::Auth0) {
        evaluation.mutation = true;
        if evaluation.autopilot && !evaluation.production_target {
            evaluation.decision = DopplerAuth0Decision::AllowAutopilotNonProd;
            return evaluation;
        }
        block_evaluation(
            &mut evaluation,
            "🔴 [Doppler/Auth0 Gate] BLOCKED: Auth0 operations require explicit user permission \
             in Planned mode, or a non-prod tenant in Autopilot. Production Auth0 changes always \
             require Gary's explicit approval — no exceptions, even in Autopilot."
                .to_string(),
        );
        return evaluation;
    }

    // Doppler — block mutations, allow reads
    if matches!(provider, DopplerAuth0Provider::Doppler) {
        let op = operation.unwrap_or("");
        // Allow read-only operations
        if DOPPLER_READ_OPS.contains(&op) {
            evaluation.read_only = true;
            evaluation.decision = DopplerAuth0Decision::AllowReadOnly;
            return evaluation;
        }
        evaluation.mutation = true;

        // Autopilot authorization — allow Doppler mutations against non-prod configs
        // without requiring an override. Prod configs (config/project name
        // contains `prd`/`prod`/`production`) still require the explicit
        // override path below.
        if evaluation.autopilot && !evaluation.production_target {
            evaluation.decision = DopplerAuth0Decision::AllowAutopilotNonProd;
            return evaluation;
        }

        if session_doppler_override_active(input, ctx) {
            evaluation.signed_override_active = true;
            evaluation.decision = DopplerAuth0Decision::AllowSignedOverride;
            return evaluation;
        }

        // Block everything else (mutations)
        block_evaluation(&mut evaluation, format!(
            "🔴 [Doppler/Auth0 Gate] BLOCKED: Doppler mutation `{op}` requires explicit user permission. \
             Ask Gary before making ANY changes to Doppler secrets or configuration. NO EXCEPTIONS. \
             To unblock briefly, Gary must type an \
             override phrase like \"override doppler\" or \"authorize doppler write\" in his next prompt."
        ));
        return evaluation;
    }

    evaluation
}

fn base_evaluation(
    tool: Option<String>,
    operation: Option<String>,
    provider: DopplerAuth0Provider,
    ctx: &HookContext<'_>,
) -> DopplerAuth0Evaluation {
    DopplerAuth0Evaluation {
        tool,
        operation,
        provider,
        router_management: false,
        read_only: false,
        mutation: false,
        autopilot: is_autopilot(ctx.env),
        tool_input_present: false,
        production_target: false,
        session_id_present: false,
        signed_override_active: false,
        auth0_override_supported: false,
        should_block: false,
        decision: DopplerAuth0Decision::Allow,
        block_reason: None,
    }
}

fn provider_for_tool(tool: &str) -> DopplerAuth0Provider {
    if tool.starts_with("mcp__doppler__") {
        DopplerAuth0Provider::Doppler
    } else if tool.starts_with("mcp__auth0__") {
        DopplerAuth0Provider::Auth0
    } else {
        DopplerAuth0Provider::None
    }
}

fn operation_for_tool(tool: &str, provider: DopplerAuth0Provider) -> Option<&str> {
    match provider {
        DopplerAuth0Provider::Doppler => tool.strip_prefix("mcp__doppler__"),
        DopplerAuth0Provider::Auth0 => tool.strip_prefix("mcp__auth0__"),
        DopplerAuth0Provider::None => None,
    }
}

fn session_doppler_override_active(input: &HookInput, ctx: &HookContext<'_>) -> bool {
    let Some(session_id) = input.session_id.as_deref().filter(|id| !id.is_empty()) else {
        return false;
    };
    let path = hygiene_override::doppler_override_path(ctx.fs, session_id);
    hygiene_override::is_signed_override_active(ctx.fs, &path, "doppler", session_id)
}

fn block_evaluation(evaluation: &mut DopplerAuth0Evaluation, reason: String) {
    evaluation.should_block = true;
    evaluation.decision = DopplerAuth0Decision::Block;
    evaluation.block_reason = Some(reason);
}

pub fn output_from_evaluation(evaluation: &DopplerAuth0Evaluation) -> HookOutput {
    if evaluation.should_block {
        return HookOutput::deny(
            evaluation
                .block_reason
                .clone()
                .unwrap_or_else(|| "Doppler/Auth0 gate blocked without a reason".to_string()),
        );
    }
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{
        stub_ctx, StubEnv, StubFs, StubGit, StubMemoryMcp, StubProcess,
    };

    /// Build a `HookContext` with `SENTINEL_AUTOPILOT=1` injected via `StubEnv`
    /// so tests don't have to mutate process-global env state.
    fn ctx_autopilot_on() -> HookContext<'static> {
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let fs: &'static StubFs = Box::leak(Box::new(StubFs));
        let process: &'static StubProcess = Box::leak(Box::new(StubProcess));
        let memory_mcp: &'static StubMemoryMcp = Box::leak(Box::new(StubMemoryMcp));
        let env: &'static StubEnv =
            Box::leak(Box::new(StubEnv::with(&[("SENTINEL_AUTOPILOT", "1")])));
        HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        }
    }

    fn input_with_tool(tool: &str) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            ..Default::default()
        }
    }

    fn input_with_tool_and_args(tool: &str, args: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            tool_input: Some(args),
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
        assert!(process(&input_with_tool("mcp__linear__create_issue"), &ctx)
            .blocked
            .is_none());
    }

    #[test]
    fn test_blocks_auth0_mutation_tools() {
        let ctx = stub_ctx();
        assert_eq!(
            process(&input_with_tool("mcp__auth0__authenticate"), &ctx).blocked,
            Some(true)
        );
    }

    #[test]
    fn test_allows_mcp_router_tools_on_all_servers() {
        let ctx = stub_ctx();
        assert!(
            process(&input_with_tool("mcp__auth0__mcp_health_check"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__auth0__mcp_list_servers"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__auth0__mcp_restart_server"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__doppler__mcp_health_check"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__doppler__mcp_list_servers"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__doppler__mcp_restart_server"), &ctx)
                .blocked
                .is_none()
        );
    }

    #[test]
    fn test_allows_doppler_read_ops() {
        let ctx = stub_ctx();
        assert!(process(&input_with_tool("mcp__doppler__get_secret"), &ctx)
            .blocked
            .is_none());
        assert!(
            process(&input_with_tool("mcp__doppler__list_projects"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__doppler__list_secrets"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__doppler__download_secrets"), &ctx)
                .blocked
                .is_none()
        );
        assert!(
            process(&input_with_tool("mcp__doppler__current_account"), &ctx)
                .blocked
                .is_none()
        );
    }

    #[test]
    fn test_blocks_doppler_mutations() {
        let ctx = stub_ctx();
        assert_eq!(
            process(&input_with_tool("mcp__doppler__set_secret"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__set_secrets"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__delete_secret"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__create_project"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__delete_config"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__lock_config"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__rollback_config"), &ctx).blocked,
            Some(true)
        );
        assert_eq!(
            process(&input_with_tool("mcp__doppler__create_service_token"), &ctx).blocked,
            Some(true)
        );
    }

    // ───────────────────────── Autopilot bypass ─────────────────────────

    #[test]
    fn test_autopilot_allows_doppler_mutation_on_nonprod_config() {
        let ctx = ctx_autopilot_on();
        let input = input_with_tool_and_args(
            "mcp__doppler__set_secret",
            serde_json::json!({
                "project": "firefly-pro-crm",
                "config": "dev",
                "name": "FOO",
                "value": "bar"
            }),
        );
        let out = process(&input, &ctx);
        assert!(
            out.blocked.is_none(),
            "autopilot + non-prod config should allow doppler mutation"
        );
    }

    #[test]
    fn test_autopilot_blocks_doppler_mutation_on_prod_config() {
        let ctx = ctx_autopilot_on();
        let input = input_with_tool_and_args(
            "mcp__doppler__set_secret",
            serde_json::json!({
                "project": "firefly-pro-crm",
                "config": "prd",
                "name": "FOO",
                "value": "bar"
            }),
        );
        let out = process(&input, &ctx);
        assert_eq!(
            out.blocked,
            Some(true),
            "autopilot must NOT bypass the gate when config is prod"
        );
    }

    #[test]
    fn test_autopilot_blocks_doppler_mutation_on_production_substring() {
        let ctx = ctx_autopilot_on();
        // A config literally named "production" — should still block.
        let input = input_with_tool_and_args(
            "mcp__doppler__set_secret",
            serde_json::json!({"project": "x", "config": "production"}),
        );
        let out = process(&input, &ctx);
        assert_eq!(out.blocked, Some(true));
    }

    #[test]
    fn test_autopilot_blocks_doppler_mutation_when_no_args_present() {
        // No `tool_input` means there is no concrete non-prod scope evidence.
        let ctx = ctx_autopilot_on();
        let out = process(&input_with_tool("mcp__doppler__set_secret"), &ctx);
        assert_eq!(out.blocked, Some(true));
    }

    #[test]
    fn test_autopilot_allows_auth0_mutation_on_nonprod_tenant() {
        let ctx = ctx_autopilot_on();
        let input = input_with_tool_and_args(
            "mcp__auth0__update_rule",
            serde_json::json!({"domain": "dev-fireflypro.us.auth0.com"}),
        );
        let out = process(&input, &ctx);
        assert!(
            out.blocked.is_none(),
            "autopilot + non-prod Auth0 tenant should allow the mutation"
        );
    }

    #[test]
    fn test_autopilot_blocks_auth0_mutation_on_prod_tenant() {
        let ctx = ctx_autopilot_on();
        let input = input_with_tool_and_args(
            "mcp__auth0__update_rule",
            serde_json::json!({"domain": "fireflypro-production.us.auth0.com"}),
        );
        let out = process(&input, &ctx);
        assert_eq!(
            out.blocked,
            Some(true),
            "autopilot must NOT bypass the gate when Auth0 tenant is production"
        );
    }

    #[test]
    fn test_autopilot_blocks_auth0_mutation_when_no_args() {
        let ctx = ctx_autopilot_on();
        let out = process(&input_with_tool("mcp__auth0__create_user"), &ctx);
        assert_eq!(
            out.blocked,
            Some(true),
            "no args means no concrete non-prod scope evidence"
        );
    }

    #[test]
    fn targets_production_matches_common_prod_markers() {
        let prod = serde_json::json!({"config": "PROD"});
        assert!(targets_production(Some(&prod)));

        let prd = serde_json::json!({"config": "prd"});
        assert!(targets_production(Some(&prd)));

        let production = serde_json::json!({"config": "production"});
        assert!(targets_production(Some(&production)));

        let domain_prod = serde_json::json!({"domain": "app.production.example.com"});
        assert!(targets_production(Some(&domain_prod)));

        let dev = serde_json::json!({"config": "dev"});
        assert!(!targets_production(Some(&dev)));

        let stg = serde_json::json!({"config": "stg"});
        assert!(!targets_production(Some(&stg)));

        let no_match = serde_json::json!({"config": "local-dev"});
        assert!(!targets_production(Some(&no_match)));

        let missing = serde_json::json!({"unrelated": "prod"});
        assert!(!targets_production(Some(&missing)));

        // None input → conservative true.
        assert!(targets_production(None));
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
