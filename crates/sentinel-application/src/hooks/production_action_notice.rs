//! `production_action_notice` — `PreToolUse` hook that surfaces a
//! **dual-display** notice the moment the agent runs a production-touching
//! tool call *while the session-wide production override is armed*.
//!
//! This is the action-time companion to [`super::production_override`]: that
//! hook owns the ARM/REVOKE state machine and the arm/lock transition notice;
//! this one fires on the *tool boundary* so the operator SEES each prod action
//! as it happens, instead of relying on the agent remembering to announce it
//! per the generated-CLAUDE.md policy. Making it a hook is the point — sentinel
//! enforces behavior structurally rather than trusting the model to self-report.
//!
//! Behavior:
//! - **No-op unless armed.** If `SessionState.production_override` is not armed,
//!   this hook does nothing (zero cost for everyone who never arms). When the
//!   override is locked the prod action is refused by policy/other gates anyway,
//!   so a "prod authorized" notice would be wrong.
//! - **Non-blocking.** It always returns [`HookOutput::allow`]; it only decorates
//!   the output with `system_message` (operator terminal) + `additional_context`
//!   (model). A false positive is one extra notice line, never a deadlock.
//! - **Heuristic, deliberately additive.** It fires when the tool is a *mutating*
//!   surface (Bash, or an MCP tool whose name carries a write verb) AND the tool
//!   input mentions a delimited production marker (`prod` / `prd` / `production`).
//!   Reads (`get` / `list` / `health` …) and non-prod calls stay silent. A false
//!   negative just falls back to the policy-driven announce — no harm.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput, HookSpecificOutput};
use sentinel_domain::state::SessionState;

/// Write verbs — the leading word of an MCP tool's action denotes a mutation.
/// Matched against the *leading token* of the tool's final `__`-segment (e.g.
/// `mcp__doppler__set_secret` → `set`), NOT as a substring, so `deploy` here
/// never collides with a read name like `list_deployments`.
const WRITE_VERBS: &[&str] = &[
    "deploy", "promote", "redeploy", "rollback", "migrate", "set", "create", "delete", "update",
    "remove", "destroy", "drop", "scale", "restart", "publish", "push", "add", "clone", "rotate",
    "revoke", "import", "apply", "trigger", "invoke", "merge",
];

/// Read verbs — leading words that denote a non-mutating query. Matched the
/// same leading-token way (`mcp__vercel__list_deployments` → `list`).
const READ_VERBS: &[&str] = &[
    "get", "list", "describe", "fetch", "read", "search", "query", "view", "check", "health",
    "current", "count", "test", "verify", "is", "find", "show", "poll", "whoami",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionActionNoticeDecision {
    AllowSilent,
    Notice,
}

#[derive(Debug, Clone)]
pub struct ProductionActionNoticeEvaluation {
    pub production_override_armed: bool,
    pub tool: Option<String>,
    pub tool_present: bool,
    pub pure_read: bool,
    pub mutating_tool: bool,
    pub tool_input_present: bool,
    pub file_path_present: bool,
    pub haystack_present: bool,
    pub haystack: String,
    pub mentions_prod: bool,
    pub should_notice: bool,
    pub decision: ProductionActionNoticeDecision,
}

impl ProductionActionNoticeEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.production_override_armed && self.tool_present && self.mutating_tool && !self.pure_read
    }
}

/// Extract the action verb of a tool name: the leading word of its final
/// `__`-separated segment, lowercased. `mcp__vercel__list_deployments` → `list`;
/// `set_secret` → `set`; `Bash` → `bash`.
fn action_verb(tool_name: &str) -> String {
    let segment = tool_name.rsplit("__").next().unwrap_or(tool_name);
    segment.split('_').next().unwrap_or(segment).to_lowercase()
}

/// Does the tool name denote a mutating action? `Bash` always qualifies (it can
/// run anything). For MCP tools, qualify when the leading verb is a write verb.
#[must_use]
pub fn is_mutating_tool(tool_name: &str) -> bool {
    if tool_name == "Bash" {
        return true;
    }
    let verb = action_verb(tool_name);
    WRITE_VERBS.contains(&verb.as_str())
}

/// Is the tool a pure read (leading verb is a read verb and not a write verb)?
/// Used to suppress notices on obvious read calls regardless of prod mentions.
#[must_use]
pub fn is_pure_read(tool_name: &str) -> bool {
    if tool_name == "Bash" {
        return false;
    }
    let verb = action_verb(tool_name);
    READ_VERBS.contains(&verb.as_str()) && !WRITE_VERBS.contains(&verb.as_str())
}

/// Does the text mention a delimited production marker? Matches `production`
/// anywhere, and `prod` / `prd` only as a delimited token so we don't fire on
/// `product`, `reproduce`, or `productivity`. Case-insensitive; caller passes
/// already-lowercased text.
#[must_use]
pub fn mentions_prod(text_lower: &str) -> bool {
    if text_lower.contains("production") {
        return true;
    }
    // Delimited prod / prd: bounded by start/end or a non-alphanumeric char.
    let bytes = text_lower.as_bytes();
    for token in ["prod", "prd"] {
        let mut from = 0usize;
        while let Some(rel) = text_lower[from..].find(token) {
            let start = from + rel;
            let end = start + token.len();
            let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

/// Build the dual-display notice `(human, model)` naming the tool.
#[must_use]
pub fn format_action_notice(tool_name: &str) -> (String, String) {
    (
        format!(
            "🔓 [prod] {tool_name} touching production — authorized via the armed \
             session override. Say \"production lock\" to re-lock."
        ),
        format!(
            "[ProductionOverride] Production-touching action via `{tool_name}` is \
             running under the armed session override — authorized, proceeding. \
             This notice is the operator's per-action audit trail; no extra \
             confirmation is required while armed."
        ),
    )
}

/// Process a `PreToolUse`. Emits a non-blocking dual-display notice when the
/// production override is armed AND the call looks like a production-touching
/// mutating action. Silent otherwise.
#[must_use]
pub fn process(input: &HookInput, state: &SessionState) -> HookOutput {
    let evaluation = evaluate(input, state.production_override_armed());
    output_from_evaluation(&evaluation)
}

#[must_use]
pub fn evaluate(
    input: &HookInput,
    production_override_armed: bool,
) -> ProductionActionNoticeEvaluation {
    let mut evaluation = ProductionActionNoticeEvaluation {
        production_override_armed,
        tool: input.tool_name.clone(),
        tool_present: input
            .tool_name
            .as_deref()
            .is_some_and(|tool| !tool.is_empty()),
        pure_read: false,
        mutating_tool: false,
        tool_input_present: input.tool_input.is_some(),
        file_path_present: input
            .file_path
            .as_deref()
            .is_some_and(|path| !path.is_empty()),
        haystack_present: false,
        haystack: String::new(),
        mentions_prod: false,
        should_notice: false,
        decision: ProductionActionNoticeDecision::AllowSilent,
    };

    // Cheapest guard first: do nothing unless prod work is armed.
    if !production_override_armed {
        return evaluation;
    }
    let Some(tool_name) = input.tool_name.as_deref() else {
        return evaluation;
    };
    evaluation.pure_read = is_pure_read(tool_name);
    evaluation.mutating_tool = is_mutating_tool(tool_name);
    if is_pure_read(tool_name) || !is_mutating_tool(tool_name) {
        return evaluation;
    }
    // Scan the tool input (and file_path, for Write/Edit) for a prod marker.
    let mut haystack = input
        .tool_input
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    if let Some(fp) = input.file_path.as_deref() {
        haystack.push(' ');
        haystack.push_str(fp);
    }
    evaluation.haystack_present = !haystack.is_empty();
    evaluation.mentions_prod = mentions_prod(&haystack.to_lowercase());
    evaluation.haystack = haystack;
    evaluation.should_notice = evaluation.mentions_prod;
    if evaluation.should_notice {
        evaluation.decision = ProductionActionNoticeDecision::Notice;
    }
    evaluation
}

#[must_use]
pub fn output_from_evaluation(evaluation: &ProductionActionNoticeEvaluation) -> HookOutput {
    match evaluation.decision {
        ProductionActionNoticeDecision::AllowSilent => HookOutput::allow(),
        ProductionActionNoticeDecision::Notice => {
            let Some(tool_name) = evaluation
                .tool
                .as_deref()
                .filter(|tool_name| !tool_name.trim().is_empty())
            else {
                return HookOutput::deny(
                    "[Sentinel-Authority] production_action_notice: refusing unaudited \
                     production action notice — missing concrete tool identity.",
                );
            };
            let (human, model) = format_action_notice(tool_name);
            let mut out = HookOutput::allow();
            out.system_message = Some(human);
            out.hook_specific_output = Some(HookSpecificOutput {
                hook_event_name: HookEvent::PreToolUse.to_string(),
                additional_context: Some(model),
                ..Default::default()
            });
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre_input(tool: &str, json: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            tool_input: Some(json),
            ..Default::default()
        }
    }

    fn armed() -> SessionState {
        let mut s = SessionState::new("s");
        s.arm_production_override(None);
        s
    }

    #[test]
    fn silent_when_not_armed() {
        let state = SessionState::new("s");
        let out = process(
            &pre_input(
                "Bash",
                serde_json::json!({"command": "deploy to production"}),
            ),
            &state,
        );
        assert!(out.system_message.is_none());
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn fires_on_bash_prod_command_when_armed() {
        let state = armed();
        let out = process(
            &pre_input(
                "Bash",
                serde_json::json!({"command": "wrangler deploy --env production"}),
            ),
            &state,
        );
        assert!(out.system_message.is_some(), "human channel set");
        let ctx = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("model channel set");
        assert!(ctx.contains("ProductionOverride"));
    }

    #[test]
    fn silent_on_non_prod_bash_when_armed() {
        let state = armed();
        let out = process(
            &pre_input(
                "Bash",
                serde_json::json!({"command": "cargo test --workspace"}),
            ),
            &state,
        );
        assert!(out.system_message.is_none());
    }

    #[test]
    fn fires_on_mcp_write_tool_with_prod_input() {
        let state = armed();
        let out = process(
            &pre_input(
                "mcp__doppler__set_secret",
                serde_json::json!({"config": "prd", "name": "API_KEY"}),
            ),
            &state,
        );
        assert!(out.system_message.is_some());
    }

    #[test]
    fn silent_on_mcp_read_tool_even_with_prod_input() {
        // A read against prod must NOT notify (get_project on prod config).
        let state = armed();
        let out = process(
            &pre_input(
                "mcp__vercel__get_project",
                serde_json::json!({"env": "production"}),
            ),
            &state,
        );
        assert!(out.system_message.is_none(), "reads never notify");
    }

    #[test]
    fn prod_token_is_delimited_not_substring() {
        // "product" / "reproduce" must not trip the prod matcher.
        assert!(!mentions_prod("update the product catalog"));
        assert!(!mentions_prod("reproduce the bug locally"));
        assert!(mentions_prod("deploy to prod"));
        assert!(mentions_prod("config=prd"));
        assert!(mentions_prod("the production database"));
    }

    #[test]
    fn write_verb_in_name_is_mutating_read_verb_is_not() {
        assert!(is_mutating_tool("mcp__railway__create_service"));
        assert!(is_mutating_tool("Bash"));
        assert!(is_pure_read("mcp__vercel__list_deployments"));
        assert!(!is_mutating_tool("mcp__vercel__list_deployments"));
        // update_* has a write verb even though "status" is a read fragment.
        assert!(is_mutating_tool("mcp__linear__update_issue"));
        assert!(!is_pure_read("mcp__linear__update_issue"));
    }

    #[test]
    fn fires_on_write_edit_with_prod_file_path() {
        let state = armed();
        let mut input = pre_input("create_dns_record", serde_json::json!({"type": "A"}));
        input.file_path = Some("/etc/production/secrets.env".to_string());
        let out = process(&input, &state);
        assert!(out.system_message.is_some());
    }

    #[test]
    fn notice_output_requires_concrete_tool_identity() {
        let evaluation = ProductionActionNoticeEvaluation {
            production_override_armed: true,
            tool: None,
            tool_present: false,
            pure_read: false,
            mutating_tool: true,
            tool_input_present: true,
            file_path_present: false,
            haystack_present: true,
            haystack: "deploy production".to_string(),
            mentions_prod: true,
            should_notice: true,
            decision: ProductionActionNoticeDecision::Notice,
        };

        let out = output_from_evaluation(&evaluation);
        let decision = out
            .hook_specific_output
            .as_ref()
            .and_then(|hook| hook.permission_decision);
        assert!(matches!(
            decision,
            Some(sentinel_domain::events::PermissionDecision::Deny)
        ));
        let reason = out
            .hook_specific_output
            .as_ref()
            .and_then(|hook| hook.permission_decision_reason.as_deref())
            .expect("deny reason");
        assert!(reason.contains("missing concrete tool identity"));
        assert!(!reason.contains("unknown"));
    }
}
