//! `AskUserQuestion` task-resync nudge.
//!
//! `PostToolUse` hook keyed on the `AskUserQuestion` tool. When the
//! user's answer looks **direction-changing** — i.e. it likely altered
//! the approach rather than confirming an existing one — the hook
//! injects an advisory reminder via `additionalContext` telling the
//! agent to re-sync the affected task subtree before the next unit of
//! work.
//!
//! **Why this exists.** A decision made via `AskUserQuestion` once
//! changed the approach mid-session, but the child subtask
//! *descriptions* kept stale old-approach text and silently drifted
//! (the parent checklist got updated; the leaf tasks did not). A
//! deterministic hook can't judge semantic drift, but it CAN nudge on
//! the trigger event so the model re-reads and re-syncs the subtree.
//!
//! **Soft nudge only — fail-open.** This hook NEVER denies, errors, or
//! blocks. A parse error or a missing field results in a plain
//! `allow()` with no injection. A false-positive reminder is cheap (we
//! re-confirm a subtree that was already fine); a missed drift is the
//! actual failure mode, so the heuristic deliberately leans toward
//! emitting when uncertain. It does NOT fire on every `AskUserQuestion`
//! blindly — the heuristic below gates it.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// The tool we key on.
const TOOL: &str = "AskUserQuestion";

/// Phrases in the *answer text* that signal a change of direction.
/// Case-insensitive substring match. Each is chosen to have little
/// innocent reading when it appears in a decision the user just made.
const ANSWER_PIVOT_PHRASES: &[&str] = &[
    "instead",
    "pivot",
    "rebuild",
    "rewrite",
    "switch to",
    "different approach",
    "scrap",
    "redo",
    "hard copy",
    "start over",
];

/// Phrases in the *question header / question text* that signal the
/// question itself was about approach/strategy/scope/direction — i.e.
/// any answer to it is more likely to be direction-setting.
const QUESTION_DIRECTION_PHRASES: &[&str] = &[
    "approach",
    "strategy",
    "scope",
    "direction",
    "how should",
    " vs ",
    " vs.",
];

/// The advisory reminder injected when the heuristic fires.
const RESYNC_REMINDER: &str = "[Sentinel — Task Resync] A decision was just made via \
AskUserQuestion. If it changed the approach, re-sync the affected task subtree NOW — \
update each child task's subject AND description (not just the parent checklist), and \
delete superseded task chains — before the next unit of work.";

/// `PostToolUse` entry point. Returns `allow()` with an
/// `additionalContext` injection when the answer looks
/// direction-changing; plain `allow()` (no injection) otherwise.
///
/// Fail-open contract: any missing/None field → `allow()` with no
/// injection. This never blocks or errors.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    // Only fire for AskUserQuestion. Everything else: silent allow.
    if input.tool_name.as_deref() != Some(TOOL) {
        return HookOutput::allow();
    }

    if !is_direction_changing(input.tool_input.as_ref(), input.tool_result.as_ref()) {
        return HookOutput::allow();
    }

    tracing::info!("ask_question_resync_nudge: direction-changing answer → injecting resync nudge");
    HookOutput::inject_context(HookEvent::PostToolUse, RESYNC_REMINDER)
}

/// The heuristic. Returns `true` (→ emit the nudge) if ANY of:
///
/// 1. **Non-recommended selection** — the selected option is not the
///    first/recommended option for its question (Claude Code orders
///    options with the recommended one first). Only used when the
///    selection's position is extractable from the payload.
/// 2. **Pivot phrase in the answer text** — the user's answer contains
///    one of [`ANSWER_PIVOT_PHRASES`].
/// 3. **Direction-shaped question** — a question header or body
///    contains one of [`QUESTION_DIRECTION_PHRASES`].
///
/// When option-order isn't extractable we fall back to keyword/header
/// matching only. When uncertain, we lean toward emitting — a
/// false-positive reminder is cheap; missed drift is the failure.
fn is_direction_changing(
    tool_input: Option<&serde_json::Value>,
    tool_result: Option<&serde_json::Value>,
) -> bool {
    // (1) Structured: a non-first option was selected.
    if selected_non_recommended_option(tool_input, tool_result) {
        return true;
    }

    // (2) Pivot phrase anywhere in the answer text.
    if let Some(result) = tool_result {
        let answer_text = flatten_strings(result).to_ascii_lowercase();
        if ANSWER_PIVOT_PHRASES.iter().any(|p| answer_text.contains(p)) {
            return true;
        }
    }

    // (3) Direction-shaped question (header or body). Scan the input
    // questions' header/question fields specifically, falling back to a
    // full flatten so we still catch the phrases if the schema shifts.
    if let Some(input) = tool_input {
        let question_text = question_header_and_body_text(input).to_ascii_lowercase();
        if QUESTION_DIRECTION_PHRASES
            .iter()
            .any(|p| question_text.contains(p))
        {
            return true;
        }
    }

    false
}

/// Attempt to determine whether the user selected an option OTHER than
/// the first (recommended) one for any question.
///
/// Claude Code's `AskUserQuestion` input is shaped roughly:
/// ```json
/// { "questions": [
///     { "question": "...", "header": "...", "multiSelect": false,
///       "options": [ {"label": "A", ...}, {"label": "B", ...} ] }
/// ]}
/// ```
/// and the result carries the chosen answer label(s) per question. The
/// exact result key has varied across Claude Code versions, so we match
/// the selected label against the option list robustly:
///
/// - For each question, take `options[0].label` (the recommended one).
/// - Collect every chosen answer label we can find in the result.
/// - If a chosen label matches an option label that is NOT the first
///   for its question, that's a non-recommended selection → `true`.
///
/// Returns `false` (not `true`) on any ambiguity or missing data — the
/// keyword heuristics in [`is_direction_changing`] still get their turn,
/// so a `false` here is not the final word. This keeps option-order
/// strictly additive and never a false *negative* of the whole hook.
fn selected_non_recommended_option(
    tool_input: Option<&serde_json::Value>,
    tool_result: Option<&serde_json::Value>,
) -> bool {
    let (Some(input), Some(result)) = (tool_input, tool_result) else {
        return false;
    };
    let Some(questions) = input.get("questions").and_then(|q| q.as_array()) else {
        return false;
    };

    // All answer strings present anywhere in the result, lowercased.
    let chosen = collect_answer_labels(result);
    if chosen.is_empty() {
        return false;
    }

    for q in questions {
        let Some(options) = q.get("options").and_then(|o| o.as_array()) else {
            continue;
        };
        if options.len() < 2 {
            // Single-option (or empty) question can't be "non-first".
            continue;
        }
        // Recommended = first option label.
        let recommended = options
            .first()
            .and_then(option_label)
            .map(|s| s.to_ascii_lowercase());
        for (idx, opt) in options.iter().enumerate() {
            let Some(label) = option_label(opt).map(|s| s.to_ascii_lowercase()) else {
                continue;
            };
            // Did the user choose this option's label?
            if chosen.iter().any(|c| c == &label) {
                // It counts as direction-changing only if it's NOT the
                // recommended (first) option.
                let is_recommended = idx == 0 || Some(&label) == recommended.as_ref();
                if !is_recommended {
                    return true;
                }
            }
        }
    }
    false
}

/// Extract an option's display label from its JSON object. Options are
/// objects like `{"label": "...", "description": "..."}`, but some
/// payloads use a bare string — handle both.
fn option_label(opt: &serde_json::Value) -> Option<String> {
    if let Some(s) = opt.as_str() {
        return Some(s.to_string());
    }
    opt.get("label")
        .and_then(|l| l.as_str())
        .map(ToString::to_string)
}

/// Collect every plausible "chosen answer label" string from the result
/// JSON, lowercased. We don't rely on a single key name because the
/// result schema has shifted across Claude Code versions; instead we
/// gather the string values under common answer-bearing keys
/// (`answer`, `label`, `selectedOption`, `selected`, `value`), and as a
/// last resort fall back to every string in the result.
fn collect_answer_labels(result: &serde_json::Value) -> Vec<String> {
    const ANSWER_KEYS: &[&str] = &["answer", "label", "selectedOption", "selected", "value"];
    let mut out = Vec::new();
    collect_by_keys(result, ANSWER_KEYS, &mut out);
    if out.is_empty() {
        // Fallback: every string in the result. Broad on purpose — this
        // only feeds an exact-equality match against option labels, so
        // stray strings rarely collide with a real label.
        let flat = flatten_strings(result);
        out.extend(flat.lines().map(|l| l.trim().to_ascii_lowercase()));
    }
    out.retain(|s| !s.is_empty());
    out
}

/// Recursively collect string values stored under any of `keys`.
fn collect_by_keys(value: &serde_json::Value, keys: &[&str], out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                if keys.iter().any(|key| k.eq_ignore_ascii_case(key)) {
                    if let Some(s) = v.as_str() {
                        out.push(s.trim().to_ascii_lowercase());
                    } else if let Some(arr) = v.as_array() {
                        for item in arr {
                            if let Some(s) = item.as_str() {
                                out.push(s.trim().to_ascii_lowercase());
                            }
                        }
                    }
                }
                collect_by_keys(v, keys, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_by_keys(v, keys, out);
            }
        }
        _ => {}
    }
}

/// Concatenate the `header` and `question` text fields of every
/// question in the input, plus a full string flatten as a fallback so a
/// schema shift never silently drops the signal.
fn question_header_and_body_text(input: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(questions) = input.get("questions").and_then(|q| q.as_array()) {
        for q in questions {
            for field in ["header", "question"] {
                if let Some(s) = q.get(field).and_then(|v| v.as_str()) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
    }
    // Fallback / belt-and-suspenders: also include the full flatten.
    out.push_str(&flatten_strings(input));
    out
}

/// Flatten all string values in a JSON tree into one newline-joined
/// string. Mirrors the approach in `prompt_injection_nudge` — keys and
/// non-string scalars carry no useful signal for our matching.
fn flatten_strings(value: &serde_json::Value) -> String {
    let mut out = String::new();
    accumulate_strings(value, &mut out);
    out
}

fn accumulate_strings(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                accumulate_strings(v, out);
            }
        }
        serde_json::Value::Object(obj) => {
            for v in obj.values() {
                accumulate_strings(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ctx() -> crate::hooks::HookContext<'static> {
        crate::hooks::test_support::stub_ctx()
    }

    fn input(tool: &str, ti: serde_json::Value, tr: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool.into()),
            tool_input: Some(ti),
            tool_result: Some(tr),
            ..HookInput::default()
        }
    }

    fn injected(out: &HookOutput) -> Option<String> {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
    }

    // ── tool gating ──────────────────────────────────────────────

    #[test]
    fn non_ask_tool_never_fires() {
        let out = process(
            &HookInput {
                tool_name: Some("Bash".into()),
                tool_result: Some(json!({"answer": "switch to a totally different approach"})),
                ..HookInput::default()
            },
            &ctx(),
        );
        assert!(injected(&out).is_none());
        assert!(out.blocked.is_none());
    }

    #[test]
    fn missing_everything_is_fail_open_no_injection() {
        let out = process(
            &HookInput {
                tool_name: Some(TOOL.into()),
                ..HookInput::default()
            },
            &ctx(),
        );
        assert!(injected(&out).is_none());
        assert!(out.blocked.is_none());
    }

    // ── direction-changing → nudge ───────────────────────────────

    #[test]
    fn pivot_phrase_in_answer_fires() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "Next step", "question": "What now?"}]}),
                json!({"responses": [{"header": "Next step", "answer": "Let's rewrite it from scratch"}]}),
            ),
            &ctx(),
        );
        let body = injected(&out).expect("nudge should fire on pivot phrase");
        assert!(body.contains("re-sync the affected task subtree"));
        assert!(body.contains("AskUserQuestion"));
    }

    #[test]
    fn instead_phrase_fires() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "Plan", "question": "ok?"}]}),
                json!({"answer": "Use Postgres instead of SQLite"}),
            ),
            &ctx(),
        );
        assert!(injected(&out).is_some());
    }

    #[test]
    fn direction_shaped_question_header_fires() {
        // Mundane-looking answer, but the QUESTION was about approach.
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "Approach", "question": "Which approach should we take?", "options": [{"label": "Option A"}]}]}),
                json!({"answer": "Option A"}),
            ),
            &ctx(),
        );
        assert!(
            injected(&out).is_some(),
            "question about 'approach' should fire even with a tame answer"
        );
    }

    #[test]
    fn how_should_question_fires() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "Build", "question": "How should we structure the module?"}]}),
                json!({"answer": "Flat layout"}),
            ),
            &ctx(),
        );
        assert!(injected(&out).is_some());
    }

    #[test]
    fn non_recommended_option_selected_fires() {
        // First option is recommended; user picked the second.
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{
                    "header": "Storage",
                    "question": "Pick a store",
                    "options": [
                        {"label": "Keep current"},
                        {"label": "Move to Redis"}
                    ]
                }]}),
                json!({"responses": [{"header": "Storage", "answer": "Move to Redis"}]}),
            ),
            &ctx(),
        );
        assert!(
            injected(&out).is_some(),
            "selecting a non-first option should fire"
        );
    }

    #[test]
    fn case_insensitive_pivot_match() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "x", "question": "y"}]}),
                json!({"answer": "SCRAP all of it and START OVER"}),
            ),
            &ctx(),
        );
        assert!(injected(&out).is_some());
    }

    // ── mundane → no nudge ───────────────────────────────────────

    #[test]
    fn recommended_first_option_mundane_does_not_fire() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{
                    "header": "Confirm",
                    "question": "Proceed with the planned migration?",
                    "options": [
                        {"label": "Yes, proceed"},
                        {"label": "Cancel"}
                    ]
                }]}),
                json!({"responses": [{"header": "Confirm", "answer": "Yes, proceed"}]}),
            ),
            &ctx(),
        );
        assert!(
            injected(&out).is_none(),
            "confirming the recommended first option is not direction-changing"
        );
    }

    #[test]
    fn mundane_yes_no_no_options_does_not_fire() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "Ready", "question": "Run the tests now?"}]}),
                json!({"answer": "Yes"}),
            ),
            &ctx(),
        );
        assert!(injected(&out).is_none());
    }

    #[test]
    fn innocent_answer_without_keywords_does_not_fire() {
        let out = process(
            &input(
                TOOL,
                json!({"questions": [{"header": "Naming", "question": "What should we call the field?"}]}),
                json!({"answer": "user_email"}),
            ),
            &ctx(),
        );
        assert!(injected(&out).is_none());
    }
}
