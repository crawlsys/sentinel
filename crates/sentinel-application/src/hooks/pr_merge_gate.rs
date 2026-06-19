//! PR Merge Gate
//!
//! Warns on `gh pr merge` commands in Bash.
//! The CLAUDE.md says: "Always ask for confirmation before merging a PR.
//! No exceptions."
//!
//! This hook injects a reminder into context so Claude asks the user,
//! but does NOT hard-block the command — the user's approval in the
//! conversation is sufficient (CLAUDE.md enforces the actual rule).
//!
//! Autopilot authorization: if `SENTINEL_AUTOPILOT=1` is set, the ask prompt is
//! downgraded to an `allow` with a context-only reminder, so `gh pr merge`
//! doesn't interrupt the loop with a Yes/No dialog. Gary's CLAUDE.md still
//! requires in-conversation confirmation before hitting merge — this just
//! prevents the harness-level dialog in autopilot mode.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::EnvPort;

/// Check if autopilot mode is active via env var.
fn is_autopilot(env: &dyn EnvPort) -> bool {
    env.var("SENTINEL_AUTOPILOT")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeOperation {
    None,
    Merge,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeDecision {
    Allow,
    Ask,
    AllowAutopilotReminder,
}

#[derive(Debug, Clone)]
pub struct PrMergeEvaluation {
    pub tool: Option<String>,
    pub command: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub operation: PrMergeOperation,
    pub autopilot: bool,
    pub permission_prompt_required: bool,
    pub context_reminder_required: bool,
    pub decision: PrMergeDecision,
}

impl PrMergeEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.bash_tool && !matches!(self.operation, PrMergeOperation::None)
    }
}

/// Process a `PreToolUse` Bash event. Warns on `gh pr merge` but allows it.
pub fn process(input: &HookInput, env: &dyn EnvPort) -> HookOutput {
    let evaluation = evaluate(input, env);
    output_from_evaluation(&evaluation)
}

pub fn evaluate(input: &HookInput, env: &dyn EnvPort) -> PrMergeEvaluation {
    let tool = input.tool_name.clone();
    let bash_tool = tool.as_deref().map_or(true, |tool| tool == "Bash");
    let command = extract_bash_command(input).map(str::to_string);
    let autopilot = is_autopilot(env);
    let Some(cmd) = command.as_deref() else {
        return PrMergeEvaluation {
            tool,
            command,
            bash_tool,
            command_present: false,
            operation: PrMergeOperation::None,
            autopilot,
            permission_prompt_required: false,
            context_reminder_required: false,
            decision: PrMergeDecision::Allow,
        };
    };

    let operation = operation_for_command(cmd);
    let pr_merge_or_close = !matches!(operation, PrMergeOperation::None);
    let permission_prompt_required = bash_tool && pr_merge_or_close && !autopilot;
    let context_reminder_required = bash_tool && pr_merge_or_close && autopilot;
    let decision = if permission_prompt_required {
        PrMergeDecision::Ask
    } else if context_reminder_required {
        PrMergeDecision::AllowAutopilotReminder
    } else {
        PrMergeDecision::Allow
    };

    PrMergeEvaluation {
        tool,
        command,
        bash_tool,
        command_present: true,
        operation,
        autopilot,
        permission_prompt_required,
        context_reminder_required,
        decision,
    }
}

fn operation_for_command(cmd: &str) -> PrMergeOperation {
    if cmd.contains("gh pr merge") {
        PrMergeOperation::Merge
    } else if cmd.contains("gh pr close") {
        PrMergeOperation::Close
    } else {
        PrMergeOperation::None
    }
}

pub fn output_from_evaluation(evaluation: &PrMergeEvaluation) -> HookOutput {
    match evaluation.decision {
        PrMergeDecision::Allow => HookOutput::allow(),
        PrMergeDecision::Ask => HookOutput::ask(
            "[PR Merge Gate] Claude is attempting to merge/close a PR. Approve to proceed.",
        ),
        PrMergeDecision::AllowAutopilotReminder => HookOutput::inject_context(
            HookEvent::PreToolUse,
            "[PR Merge Gate] AUTOPILOT: allowing `gh pr merge/close` without a \
             Yes/No dialog. Verify the in-conversation confirmation was given before \
             proceeding.",
        ),
    }
}

fn extract_bash_command(input: &HookInput) -> Option<&str> {
    input.tool_input.as_ref()?.get("command")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": cmd})),
            ..Default::default()
        }
    }

    fn no_autopilot() -> crate::hooks::test_support::StubEnv {
        crate::hooks::test_support::StubEnv::new()
    }

    fn autopilot_on() -> crate::hooks::test_support::StubEnv {
        crate::hooks::test_support::StubEnv::with(&[("SENTINEL_AUTOPILOT", "1")])
    }

    fn autopilot_off() -> crate::hooks::test_support::StubEnv {
        crate::hooks::test_support::StubEnv::with(&[("SENTINEL_AUTOPILOT", "0")])
    }

    #[test]
    fn test_asks_gh_pr_merge() {
        let out = process(&bash_input("gh pr merge 123"), &no_autopilot());
        assert!(out.blocked.is_none()); // not hard-blocked
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask)
        );
    }

    #[test]
    fn test_asks_gh_pr_close() {
        let out = process(&bash_input("gh pr close 42"), &no_autopilot());
        assert!(out.blocked.is_none());
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask)
        );
    }

    #[test]
    fn test_allows_gh_pr_view() {
        assert!(process(&bash_input("gh pr view 123"), &no_autopilot())
            .blocked
            .is_none());
    }

    #[test]
    fn test_allows_gh_pr_create() {
        assert!(
            process(&bash_input("gh pr create --title test"), &no_autopilot())
                .blocked
                .is_none()
        );
    }

    #[test]
    fn test_allows_non_gh_commands() {
        assert!(process(&bash_input("git push"), &no_autopilot())
            .blocked
            .is_none());
        assert!(process(&bash_input("cargo test"), &no_autopilot())
            .blocked
            .is_none());
    }

    #[test]
    fn test_allows_no_tool_input() {
        assert!(process(&HookInput::default(), &no_autopilot())
            .blocked
            .is_none());
    }

    #[test]
    fn test_autopilot_downgrades_ask_to_context_inject() {
        let out = process(&bash_input("gh pr merge 123 --squash"), &autopilot_on());

        assert!(out.blocked.is_none());
        let hso = out.hook_specific_output.expect("output should have hso");
        // Should NOT be asking for permission.
        assert_ne!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask),
            "autopilot must not trigger the Yes/No dialog"
        );
        // Should inject a context reminder instead.
        let ctx = hso.additional_context.unwrap_or_default();
        assert!(
            ctx.contains("AUTOPILOT"),
            "expected AUTOPILOT reminder in context, got: {ctx}"
        );
    }

    #[test]
    fn test_autopilot_false_still_asks() {
        let out = process(&bash_input("gh pr merge 7"), &autopilot_off());
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask)
        );
    }

    #[test]
    fn test_no_autopilot_env_still_asks() {
        let out = process(&bash_input("gh pr close 42"), &no_autopilot());
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask)
        );
    }
}
