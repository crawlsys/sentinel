//! Plan Title Gate — the single hard-enforcement point for plan organization.
//!
//! Fires on `PreToolUse` for `ExitPlanMode`. The companion `plan_organizer`
//! (PostToolUse) files every plan under a descriptive, kebab-case name derived
//! from the plan's title heading. That derivation needs *something* to work
//! from. This gate guarantees it: it blocks `ExitPlanMode` only when the plan
//! has no derivable title at all, so a plan can never be saved under a
//! meaningless name.
//!
//! Enforcement is deliberately narrow — the common case (a plan with a real
//! `# Title` / `## Plan: …` heading, or even just a non-empty first line) sails
//! through with zero friction. We block genuinely title-less/empty plans and
//! `ExitPlanMode` calls whose plan content is absent or not inspectable.
//!
//! The block message is `[Sentinel-Authority]`-prefixed (added at the output
//! boundary) so the agent treats it as an authoritative directive to add a
//! heading and retry.

use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};

use super::HookContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTitleDecision {
    Allow,
    BlockMissingPlan,
    BlockTitleless,
}

#[derive(Debug, Clone)]
pub struct PlanTitleEvaluation {
    pub tool: Option<String>,
    pub exit_plan_mode: bool,
    pub plan_text: Option<String>,
    pub plan_text_present: bool,
    pub derivable_title: bool,
    pub title_line_present: bool,
    pub should_block: bool,
    pub decision: PlanTitleDecision,
}

impl PlanTitleEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.exit_plan_mode
    }
}

/// Extract the plan text the model is about to submit. `ExitPlanMode`'s
/// `tool_input` carries the plan under `plan`; some shapes nest it under
/// `data.plan`. Returns `None` if neither is present.
fn plan_text(input: &HookInput) -> Option<String> {
    let ti = input.tool_input.as_ref()?;
    if let Some(p) = ti.get("plan").and_then(|v| v.as_str()) {
        return Some(p.to_string());
    }
    ti.get("data")
        .and_then(|d| d.get("plan"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Does the plan content have a title we could turn into a descriptive
/// filename? Shared with `plan_organizer`'s slug derivation — kept here as the
/// canonical predicate so the gate and the organizer agree on "has a title".
///
/// Accepts, in order of preference:
///   1. A markdown heading line (`#`/`##`/`###`) whose text has >= 2 word chars
///      after stripping a leading `Plan:` label.
///   2. Failing any heading, the first non-empty, non-heading line with >= 2
///      word chars (a plan that opens with prose still has *something*).
pub fn has_derivable_title(content: &str) -> bool {
    title_line(content).is_some()
}

/// Return the raw title text (heading or first content line) if derivable.
/// `plan_organizer::descriptive_slug` slugifies whatever this returns.
pub fn title_line(content: &str) -> Option<String> {
    // First pass: prefer a markdown heading.
    for raw in content.lines() {
        let line = raw.trim();
        if let Some(rest) = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "))
            .or_else(|| line.strip_prefix("# "))
        {
            let cleaned = strip_plan_label(rest.trim());
            if word_chars(cleaned) >= 2 {
                return Some(cleaned.to_string());
            }
        }
    }
    // Second pass: first meaningful non-heading line.
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if word_chars(line) >= 2 {
            return Some(line.to_string());
        }
    }
    None
}

/// Strip a leading `Plan:` / `plan -` style label so "## Plan: Foo Bar" →
/// "Foo Bar". Case-insensitive on the `plan` word; tolerant of `:`/`-` and
/// surrounding space.
fn strip_plan_label(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("plan") {
        let rest_trimmed = rest.trim_start_matches([':', '-', ' ', '\t']);
        if rest_trimmed.len() != rest.len() || rest.is_empty() {
            // There was a separator (or nothing) after "plan" — strip it from
            // the ORIGINAL (preserve original case) by the same byte offset.
            let stripped_len = s.len() - rest_trimmed.len();
            return s[stripped_len..].trim();
        }
    }
    s
}

/// Count alphanumeric characters — our "is there real text here" measure.
fn word_chars(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphanumeric()).count()
}

/// Guidance emitted when a plan has no derivable title.
const TITLE_GUIDANCE: &str = "This plan has no derivable title, so it can't be \
filed under a descriptive name. Add a heading as the first line — \
`# <Title>` or `## Plan: <Title>` (e.g. `## Plan: Add user auth`) — then \
re-run ExitPlanMode. Sentinel auto-files plans by their title; a title-less \
plan would land under a meaningless random slug.";

/// `PreToolUse` handler. Blocks `ExitPlanMode` when the plan is title-less or
/// absent; allows everything else (including non-ExitPlanMode tools).
pub fn process(input: &HookInput, _ctx: &HookContext<'_>) -> HookOutput {
    let evaluation = evaluate(input);
    output_from_evaluation(&evaluation)
}

#[must_use]
pub fn evaluate(input: &HookInput) -> PlanTitleEvaluation {
    let tool = input.tool_name.clone();
    let exit_plan_mode = input.tool_name.as_deref() == Some("ExitPlanMode");
    if !exit_plan_mode {
        return PlanTitleEvaluation {
            tool,
            exit_plan_mode,
            plan_text: None,
            plan_text_present: false,
            derivable_title: false,
            title_line_present: false,
            should_block: false,
            decision: PlanTitleDecision::Allow,
        };
    }

    let plan_text = plan_text(input);
    let plan_text_present = plan_text.is_some();
    let title_line_present = plan_text.as_deref().and_then(title_line).is_some();
    let derivable_title = title_line_present;
    let decision = match (plan_text_present, derivable_title) {
        (false, _) => PlanTitleDecision::BlockMissingPlan,
        (true, false) => PlanTitleDecision::BlockTitleless,
        (true, true) => PlanTitleDecision::Allow,
    };
    PlanTitleEvaluation {
        tool,
        exit_plan_mode,
        plan_text,
        plan_text_present,
        derivable_title,
        title_line_present,
        should_block: !matches!(decision, PlanTitleDecision::Allow),
        decision,
    }
}

#[must_use]
pub fn output_from_evaluation(evaluation: &PlanTitleEvaluation) -> HookOutput {
    match evaluation.decision {
        PlanTitleDecision::Allow => HookOutput::allow(),
        PlanTitleDecision::BlockMissingPlan => {
            let envelope = HookEnvelope::block(
                "Plan Title Gate",
                "ExitPlanMode did not include inspectable plan text, so Sentinel cannot derive \
                 a descriptive plan filename. Re-run ExitPlanMode with a plan body whose first \
                 line is `# <Title>` or `## Plan: <Title>`.",
            );
            HookOutput::block(envelope.render())
        }
        PlanTitleDecision::BlockTitleless => {
            let envelope = HookEnvelope::block("Plan Title Gate", TITLE_GUIDANCE);
            HookOutput::block(envelope.render())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit_plan_input(plan: Option<&str>) -> HookInput {
        let mut input = HookInput {
            tool_name: Some("ExitPlanMode".to_string()),
            ..Default::default()
        };
        if let Some(p) = plan {
            input.tool_input = Some(serde_json::json!({ "plan": p }));
        }
        input
    }

    #[test]
    fn heading_title_is_derivable() {
        assert!(has_derivable_title("# Add user authentication\n\nbody"));
        assert!(has_derivable_title("## Plan: Force plan organization\n"));
        assert!(has_derivable_title("### Some Section\ntext"));
    }

    #[test]
    fn prose_first_line_is_derivable() {
        assert!(has_derivable_title(
            "We will refactor the auth module.\nmore"
        ));
    }

    #[test]
    fn empty_or_titleless_is_not_derivable() {
        assert!(!has_derivable_title(""));
        assert!(!has_derivable_title("   \n\n  \n"));
        assert!(!has_derivable_title("#\n##\n# \n")); // headings with no text
        assert!(!has_derivable_title("a\n")); // single char, < 2 word chars
    }

    #[test]
    fn title_line_strips_plan_label() {
        assert_eq!(
            title_line("## Plan: Force Plan Organization").as_deref(),
            Some("Force Plan Organization")
        );
        assert_eq!(
            title_line("# plan - jwt refactor").as_deref(),
            Some("jwt refactor")
        );
        // A heading that is literally just "Plan:" has no text after the label
        // → falls through to first content line.
        assert_eq!(
            title_line("## Plan:\nActual first line here").as_deref(),
            Some("Actual first line here")
        );
    }

    #[test]
    fn title_line_preserves_non_plan_headings() {
        assert_eq!(
            title_line("# Add User Authentication").as_deref(),
            Some("Add User Authentication")
        );
    }

    #[test]
    fn process_allows_titled_plan() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&exit_plan_input(Some("## Plan: Do the thing\nbody")), &ctx);
        assert!(out.blocked.is_none(), "titled plan must pass");
    }

    #[test]
    fn process_blocks_titleless_plan() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&exit_plan_input(Some("   \n\n")), &ctx);
        assert_eq!(out.blocked, Some(true), "title-less plan must block");
        let reason = out.reason.expect("block reason");
        assert!(reason.contains("no derivable title"));
        assert!(reason.contains("ExitPlanMode"));
    }

    #[test]
    fn process_blocks_without_plan_text() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&exit_plan_input(None), &ctx);
        assert_eq!(out.blocked, Some(true), "missing plan text must block");
        let reason = out.reason.expect("block reason");
        assert!(reason.contains("inspectable plan text"));
        assert!(reason.contains("ExitPlanMode"));
    }

    #[test]
    fn process_ignores_other_tools() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &ctx).blocked.is_none());
    }
}
