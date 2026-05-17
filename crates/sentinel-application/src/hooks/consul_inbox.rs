//! Consul inbox drain — `UserPromptSubmit` hook that pulls
//! operator-relayed instructions out of the daemon-hosted
//! legatus's inbox and injects them into Claude Code's next turn.
//!
//! The companion to `permission_denied` + `execution_log` (which
//! push events FROM Claude Code TO the operator). This closes the
//! return path: operator types "firefly please deploy staging" on
//! Telegram → consul dispatches a `RelayInstruction` to firefly's
//! legatus over WS → the sentinel daemon buffers it in an
//! in-memory inbox → this hook drains it on the next
//! `UserPromptSubmit` and merges the instruction into Claude
//! Code's context.
//!
//! **Injection mode**: `hookSpecificOutput.additionalContext`
//! with explicit priority framing. This uses Claude Code's
//! official hook output mechanism (no risk of prompt-parser
//! confusion from merging into the user's literal text) while
//! leaning on the model to treat the relayed instruction as the
//! primary ask via the wording.
//!
//! Not a 100% deterministic execution gate — the model decides
//! whether to act on the injected instruction. The framing
//! nudges; non-destructive divergence is acceptable per the
//! operator's stated reliability target (~90-95%).

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_legatus::RelayInstruction;

use crate::legatus_client::{ack_fire_and_forget, drain_inbox, note_pending_instruction};

/// `UserPromptSubmit` hook entry point.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let pending = drain_inbox();
    if pending.is_empty() {
        return HookOutput::allow();
    }
    let session_id = input.session_id.as_deref();
    for instr in &pending {
        // Per-instruction Ack ping — operator gets "firefly is on
        // it (id ...)." routed back via consul → SurfaceRouting.
        ack_fire_and_forget(instr.instruction_id);
        // Record the id so the next Stop hook fire can emit a
        // matching Result. Skip if no session id (defensive — in
        // practice every UserPromptSubmit carries one).
        if let Some(sid) = session_id {
            note_pending_instruction(sid, instr.instruction_id);
        }
    }
    let context = format_pending(&pending);
    tracing::info!(
        count = pending.len(),
        "injecting operator-relayed instructions from consul inbox",
    );
    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

/// Render the pending instructions as a single `additionalContext`
/// block. Lead with the explicit priority framing so the model
/// treats the relayed instruction(s) as the primary ask.
fn format_pending(instructions: &[RelayInstruction]) -> String {
    let mut out = String::new();
    out.push_str(
        "[INSTRUCTION RELAYED BY CONSUL — TREAT AS PRIMARY ASK]\n\
         The human commander, while away from this terminal, used a chat surface \
         (Telegram / Discord / Slack / WhatsApp) to send the following \
         instruction(s) for this session via consul. Execute these BEFORE \
         responding to anything the user typed locally on this turn. If a local \
         prompt conflicts with a relayed instruction, prefer the relayed one — \
         the commander is the source of truth.\n\n",
    );
    for (idx, instr) in instructions.iter().enumerate() {
        let destructive_tag = if instr.destructive {
            " (operator flagged as DESTRUCTIVE — confirm before acting)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{n}. instruction_id={id}{destructive_tag}\n   {content}\n\n",
            n = idx + 1,
            id = instr.instruction_id,
            content = instr.content,
        ));
    }
    out.push_str(
        "[END OF RELAYED INSTRUCTIONS] — Act on the above, then if appropriate \
         circle back to whatever the local user typed.",
    );
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use sentinel_legatus::{InstructionId, SessionId};

    use super::*;

    fn fake_instruction(content: &str, destructive: bool) -> RelayInstruction {
        RelayInstruction {
            instruction_id: InstructionId::new(),
            target_session_id: SessionId::new_v7(),
            content: content.into(),
            destructive,
        }
    }

    #[test]
    fn format_pending_leads_with_priority_framing() {
        let instructions = vec![fake_instruction("deploy staging", false)];
        let out = format_pending(&instructions);
        assert!(out.starts_with("[INSTRUCTION RELAYED BY CONSUL"));
        assert!(out.contains("deploy staging"));
        assert!(out.contains("instruction_id="));
        assert!(out.contains("[END OF RELAYED INSTRUCTIONS]"));
    }

    #[test]
    fn format_pending_marks_destructive_explicitly() {
        let instructions = vec![fake_instruction("drop the staging db", true)];
        let out = format_pending(&instructions);
        assert!(out.contains("DESTRUCTIVE"));
    }

    #[test]
    fn format_pending_numbers_multiple_instructions() {
        let instructions = vec![
            fake_instruction("first", false),
            fake_instruction("second", false),
        ];
        let out = format_pending(&instructions);
        assert!(out.contains("1. instruction_id="));
        assert!(out.contains("2. instruction_id="));
        assert!(out.contains("first"));
        assert!(out.contains("second"));
    }

    #[test]
    fn process_with_empty_inbox_returns_allow_without_context() {
        // No daemon running locally — drain returns empty — hook
        // returns allow() with no additional context.
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
        // No assertion on additional context: in CI this is None;
        // on a dev box with a live daemon hosting a real inbox,
        // there *might* be content. Either way the hook allows.
    }
}
