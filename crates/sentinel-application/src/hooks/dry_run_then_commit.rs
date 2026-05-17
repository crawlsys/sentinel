//! A3 — Dry-Run-Then-Commit hook (Phase 2).
//!
//! `PreToolUse` hook that, for actions whose A6 reversibility class is
//! `Irreversible` or `Catastrophic`, requires the acting agent to emit a
//! structured dry-run artifact and routes it through a separate-model-family
//! auditor before the action commits.
//!
//! Per the A3 design (`docs/a3-dry-run-then-commit.md` §4):
//!
//! 1. Reversibility check via [`ReversibilityClassifierPort`]. Trivially
//!    and ReversibleWithEffort are out of scope (the `tool_usage_gate`
//!    four-check stack already handles RWE; Trivially short-circuits at
//!    the gate). Only Irreversible / Catastrophic enter A3.
//! 2. Approval-marker short-circuit: if this exact action (keyed on a
//!    SHA-256 of `tool_name + canonical tool_input`) was already approved
//!    in this session, allow without re-auditing.
//! 3. Dry-run construction: extract prose fields (`_intent`,
//!    `_reasoning`, `_expected_effect`) from `tool_input`. If any are
//!    missing/blank, block with a clear message (incomplete dry-runs are
//!    not worth auditor cycles).
//! 4. Auditor call: route the [`DryRunRequest`] through the
//!    [`AuditorPort`] passed in.
//! 5. Decision tree per A3 spec §4.1 + §4.5:
//!    - `Pass` AND `confidence >= HUMAN_SAMPLE_CONFIDENCE_THRESHOLD` AND
//!      reversibility class is not `Catastrophic` → record approval
//!      marker; allow.
//!    - `Pass` but `confidence < threshold` OR class is `Catastrophic` →
//!      `deny` with a "human review required" reason (operator can use
//!      `hygiene_override` to proceed; the override is audit-logged).
//!    - `Block { reason }` → `deny` with the auditor's reason.
//!    - `Err(AuditorError)` → block per class policy: Irreversible blocks
//!      (operator retries when auditor returns); Catastrophic blocks AND
//!      explicitly tells the operator to escalate to human review.
//!
//! Phase 4 wires this hook into the production `hook_cmd.rs` dispatch
//! pipeline alongside `tool_usage_gate`. Until then this hook is
//! callable by tests + downstream phases via `process()` directly.

use std::path::PathBuf;

use sentinel_domain::dry_run::{
    AuditorAxes, AuditorDecision, AuditorError, AuditorVerdict, DryRunRequest,
};
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::{AuditorPort, ReversibilityClassifierPort};
use sentinel_domain::ReversibilityClass;

use super::FileSystemPort;

/// Confidence threshold below which the hook escalates to human review
/// regardless of the auditor's `decision`. Default per A3 spec §4.5;
/// configurable in a later phase.
pub const HUMAN_SAMPLE_CONFIDENCE_THRESHOLD: f32 = 0.85;

/// Marker file prefix for "this exact action was audited and approved
/// this session." Keyed by `{session_id}-{action_hash}` where
/// `action_hash` is the first 16 hex chars of `SHA-256(tool_name ||
/// canonical(tool_input))`. The hash truncation matches the rest of
/// sentinel's marker conventions — short enough for path-friendly
/// suffixes, long enough to avoid in-session collisions for distinct
/// actions.
const APPROVAL_MARKER_PREFIX: &str = "sentinel-a3-approved-";

/// Compute the action hash for an `(tool_name, tool_input)` pair. Uses
/// canonical JSON encoding (sorted keys via `serde_json::to_string` on a
/// `BTreeMap`-shaped re-serialization) so equivalent inputs hash the
/// same regardless of caller-side key ordering.
#[must_use]
pub fn action_hash_for(tool_name: &str, tool_input: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let canonical_input = canonicalize_json(tool_input);
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_input.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

/// Re-serialize a JSON value with sorted object keys so equivalent
/// inputs always produce the same canonical bytes. Recursive: nested
/// objects also get key-sorted.
fn canonicalize_json(value: &serde_json::Value) -> String {
    let canonical = canonicalize_value(value);
    canonical.to_string()
}

fn canonicalize_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize_value(v)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (k, v) in entries {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(canonicalize_value).collect())
        }
        other => other.clone(),
    }
}

fn approval_marker_path(session_id: &str, action_hash: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{APPROVAL_MARKER_PREFIX}{session_id}-{action_hash}"))
}

/// `true` if the auditor previously approved this exact action in this
/// session.
#[must_use]
pub fn has_dry_run_approval(
    fs: &dyn FileSystemPort,
    session_id: &str,
    action_hash: &str,
) -> bool {
    fs.exists(&approval_marker_path(session_id, action_hash))
}

/// Record that the auditor approved this action; subsequent identical
/// calls in the same session will short-circuit at step 2 above.
pub fn mark_dry_run_approved(
    fs: &dyn FileSystemPort,
    session_id: &str,
    action_hash: &str,
) {
    let path = approval_marker_path(session_id, action_hash);
    let _ = fs.write(&path, b"1");
}

/// Process a `PreToolUse` event through the A3 dry-run-then-commit gate.
///
/// Signature parallels the existing `tool_usage_gate::process` plus an
/// auditor port. Both ports are injected so tests can substitute
/// `StaticReversibilityClassifier` + a static / mock auditor.
pub fn process(
    input: &HookInput,
    fs: &dyn FileSystemPort,
    classifier: &dyn ReversibilityClassifierPort,
    auditor: &dyn AuditorPort,
) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return HookOutput::allow(),
    };

    // Step 1 — reversibility scope. Only Irreversible / Catastrophic
    // enter A3; Trivially + ReversibleWithEffort are handled upstream
    // by `tool_usage_gate`.
    let null_input = serde_json::Value::Null;
    let tool_input_ref = input.tool_input.as_ref().unwrap_or(&null_input);
    let class = classifier.classify(tool, tool_input_ref);
    if !class.at_least(ReversibilityClass::Irreversible) {
        return HookOutput::allow();
    }

    // Need a session id to scope the approval marker. Without one we
    // can't track approval state — block defensively for irreversible+.
    let session_id = match &input.session_id {
        Some(id) if !id.is_empty() => id.as_str(),
        _ => {
            return HookOutput::deny(format!(
                "🟠 [Dry-Run Auditor] BLOCKED: {class:?}-class action requires a \
                 session id for approval tracking; none provided."
            ));
        }
    };

    // Step 2 — approval-marker short-circuit. Same exact action already
    // approved this session? Skip the auditor call.
    let action_hash = action_hash_for(tool, tool_input_ref);
    if has_dry_run_approval(fs, session_id, &action_hash) {
        return HookOutput::allow();
    }

    // Step 3 — construct the dry-run artifact.
    let constructed_at = chrono::Utc::now();
    let dry_run = build_dry_run(session_id, tool, tool_input_ref, class, constructed_at);
    if !dry_run.is_complete() {
        return HookOutput::deny(format!(
            "🟠 [Dry-Run Auditor] BLOCKED: {class:?}-class action requires a dry-run \
             with `_intent`, `_reasoning`, and `_expected_effect` populated in tool_input. \
             Re-issue the tool call with those fields, or invoke `hygiene_override` to \
             bypass for legitimate edge cases."
        ));
    }

    // Step 4 — auditor scoring.
    let verdict = match auditor.score(&dry_run) {
        Ok(v) => v,
        Err(err) => return handle_auditor_error(class, &err),
    };

    // Step 5 — decision.
    decide(fs, session_id, &action_hash, class, &verdict)
}

/// Construct the dry-run request, pulling prose fields out of tool_input
/// when the agent supplied them inline (`_intent`, `_reasoning`,
/// `_expected_effect`). Absent fields stay empty; `DryRunRequest::is_complete`
/// is what the caller checks.
fn build_dry_run(
    session_id: &str,
    tool: &str,
    tool_input: &serde_json::Value,
    class: ReversibilityClass,
    constructed_at: chrono::DateTime<chrono::Utc>,
) -> DryRunRequest {
    let intent = extract_field(tool_input, "_intent");
    let reasoning = extract_field(tool_input, "_reasoning");
    let expected_effect = extract_field(tool_input, "_expected_effect");
    DryRunRequest::new(
        session_id,
        tool,
        tool_input.clone(),
        class,
        constructed_at,
    )
    .with_intent(intent)
    .with_reasoning(reasoning)
    .with_expected_effect(expected_effect)
}

fn extract_field(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn handle_auditor_error(class: ReversibilityClass, err: &AuditorError) -> HookOutput {
    let escalate = matches!(class, ReversibilityClass::Catastrophic);
    let escalate_suffix = if escalate {
        " Catastrophic-class action with unavailable auditor — escalate to human review via `hygiene_override` only if the action is truly time-critical."
    } else {
        " Retry when the auditor returns."
    };
    HookOutput::deny(format!(
        "🟠 [Dry-Run Auditor] BLOCKED: auditor error on {class:?}-class action: {err}.{escalate_suffix}"
    ))
}

fn decide(
    fs: &dyn FileSystemPort,
    session_id: &str,
    action_hash: &str,
    class: ReversibilityClass,
    verdict: &AuditorVerdict,
) -> HookOutput {
    match &verdict.decision {
        AuditorDecision::Block { reason } => HookOutput::deny(format!(
            "🟠 [Dry-Run Auditor] BLOCKED: {reason} (auditor: {}, confidence: {:.2})",
            verdict.auditor_model, verdict.confidence
        )),
        AuditorDecision::Pass => {
            let low_confidence = verdict.confidence < HUMAN_SAMPLE_CONFIDENCE_THRESHOLD;
            let catastrophic = matches!(class, ReversibilityClass::Catastrophic);
            if low_confidence || catastrophic {
                let why = if catastrophic && low_confidence {
                    "Catastrophic class + auditor confidence below threshold"
                } else if catastrophic {
                    "Catastrophic class — human spot-check always required regardless of auditor pass"
                } else {
                    "auditor confidence below threshold"
                };
                let (weakest_axis, weakest_score) = verdict.axes.weakest_axis();
                return HookOutput::deny(format!(
                    "🟠 [Dry-Run Auditor] HUMAN REVIEW REQUIRED: {why} \
                     (auditor: {}, confidence: {:.2}, weakest axis: {weakest_axis} @ {weakest_score:.2}). \
                     Auditor reasoning: {reasoning}. Invoke `hygiene_override` to proceed once you \
                     have read the dry-run and accept the trade-offs.",
                    verdict.auditor_model,
                    verdict.confidence,
                    reasoning = verdict.reasoning,
                ));
            }
            mark_dry_run_approved(fs, session_id, action_hash);
            HookOutput::allow()
        }
    }
}

#[cfg(test)]
#[allow(clippy::doc_markdown)] // test prose
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use sentinel_domain::events::HookInput;

    use super::*;
    use crate::auditor::StaticAuditor;
    use crate::reversibility_classifier::StaticReversibilityClassifier;

    /// In-memory FS for tests — only the methods `dry_run_then_commit`
    /// actually uses (`exists`, `write`) need real behavior; the rest
    /// satisfy the trait shape. Mutex (not RefCell) because the trait is
    /// `Send + Sync`.
    #[derive(Default)]
    struct MockFs {
        written: Mutex<HashSet<PathBuf>>,
    }

    impl MockFs {
        fn new() -> Self {
            Self::default()
        }
    }

    impl super::FileSystemPort for MockFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(std::env::temp_dir())
        }
        fn read_to_string(&self, _path: &Path) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn write(&self, path: &Path, _content: &[u8]) -> anyhow::Result<()> {
            self.written.lock().unwrap().insert(path.to_path_buf());
            Ok(())
        }
        fn append(&self, path: &Path, content: &[u8]) -> anyhow::Result<()> {
            self.write(path, content)
        }
        fn create_dir_all(&self, _path: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn read_dir(&self, _path: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn exists(&self, path: &Path) -> bool {
            self.written.lock().unwrap().contains(path)
        }
        fn is_dir(&self, _path: &Path) -> bool {
            false
        }
        fn metadata(&self, _path: &Path) -> anyhow::Result<std::fs::Metadata> {
            anyhow::bail!("not implemented")
        }
    }

    fn complete_tool_input() -> serde_json::Value {
        serde_json::json!({
            "command": "git push origin main",
            "_intent": "publish the release",
            "_reasoning": "tag was applied; CI is green",
            "_expected_effect": "remote main advances to current HEAD"
        })
    }

    fn irreversible_input() -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            session_id: Some("sess-1".to_string()),
            tool_input: Some(complete_tool_input()),
            ..Default::default()
        }
    }

    fn classifier_with(tool: &str, class: ReversibilityClass) -> StaticReversibilityClassifier {
        StaticReversibilityClassifier::empty().with(tool, class)
    }

    // ---- Reversibility scope ----

    #[test]
    fn trivially_classified_actions_bypass_audit() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::TriviallyReversible);
        let auditor = StaticAuditor::pass(0.99); // shouldn't be consulted
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert!(output.blocked.is_none(), "Trivially must allow without audit");
    }

    #[test]
    fn reversible_with_effort_actions_bypass_audit() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::ReversibleWithEffort);
        let auditor = StaticAuditor::pass(0.99);
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert!(output.blocked.is_none(), "RWE handled upstream by tool_usage_gate; A3 silent");
    }

    // ---- Incomplete dry-run ----

    #[test]
    fn missing_intent_field_blocks_with_clear_message() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.95);
        let mut input = irreversible_input();
        if let Some(obj) = input.tool_input.as_mut().and_then(|v| v.as_object_mut()) {
            obj.remove("_intent");
        }
        let output = process(&input, &fs, &classifier, &auditor);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(reason.contains("_intent"), "deny message names the missing field: {reason}");
    }

    #[test]
    fn missing_session_id_blocks() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.95);
        let mut input = irreversible_input();
        input.session_id = None;
        let output = process(&input, &fs, &classifier, &auditor);
        assert_eq!(output.blocked, Some(true));
    }

    // ---- Pass + high confidence + Irreversible ----

    #[test]
    fn irreversible_pass_high_confidence_allows_and_records_marker() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.95);
        let input = irreversible_input();
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(output.blocked.is_none());
        // Marker recorded so re-running the same action short-circuits.
        let null = serde_json::Value::Null;
        let tool_input_ref = input.tool_input.as_ref().unwrap_or(&null);
        let hash = action_hash_for("Bash", tool_input_ref);
        assert!(has_dry_run_approval(&fs, "sess-1", &hash));
    }

    #[test]
    fn second_call_with_same_action_short_circuits_without_audit() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        // First call uses a passing auditor → marker recorded.
        let pass_auditor = StaticAuditor::pass(0.95);
        let input = irreversible_input();
        let _ = process(&input, &fs, &classifier, &pass_auditor);
        // Second call uses an erroring auditor — but the marker should
        // short-circuit before the auditor is consulted.
        let err_auditor = StaticAuditor::err(AuditorError::Unavailable("down".into()));
        let output = process(&input, &fs, &classifier, &err_auditor);
        assert!(
            output.blocked.is_none(),
            "marker should short-circuit before auditor is consulted"
        );
    }

    // ---- Pass + low confidence (human review) ----

    #[test]
    fn pass_below_threshold_blocks_for_human_review() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.7);
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(reason.contains("HUMAN REVIEW REQUIRED"));
        assert!(reason.contains("confidence"));
    }

    // ---- Catastrophic always escalates ----

    #[test]
    fn catastrophic_pass_high_confidence_still_escalates() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Catastrophic);
        let auditor = StaticAuditor::pass(0.99);
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert_eq!(
            output.blocked,
            Some(true),
            "Catastrophic must always sample to human even on high-confidence pass"
        );
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(reason.contains("HUMAN REVIEW REQUIRED"));
        assert!(reason.contains("Catastrophic"));
    }

    // ---- Block decision ----

    #[test]
    fn block_decision_denies_with_auditor_reason() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::block("exfiltration risk");
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(reason.contains("exfiltration risk"));
    }

    // ---- AuditorError per class ----

    #[test]
    fn irreversible_auditor_error_blocks_with_retry_message() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::err(AuditorError::Unavailable("connection refused".into()));
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(reason.contains("Retry"));
        assert!(reason.contains("connection refused"));
    }

    #[test]
    fn catastrophic_auditor_error_blocks_and_explicitly_escalates() {
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Catastrophic);
        let auditor = StaticAuditor::err(AuditorError::TimedOut(std::time::Duration::from_secs(30)));
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(reason.contains("escalate to human"));
    }

    // ---- action_hash_for stability ----

    #[test]
    fn action_hash_stable_across_key_order_in_tool_input() {
        let a = serde_json::json!({ "a": 1, "b": 2 });
        let b = serde_json::json!({ "b": 2, "a": 1 });
        assert_eq!(
            action_hash_for("Bash", &a),
            action_hash_for("Bash", &b),
            "canonical-JSON hash should not depend on serde_json::Map iteration order"
        );
    }

    #[test]
    fn action_hash_differs_for_different_tools() {
        let same_input = serde_json::json!({ "x": 1 });
        assert_ne!(
            action_hash_for("Bash", &same_input),
            action_hash_for("Edit", &same_input)
        );
    }

    #[test]
    fn action_hash_differs_for_different_inputs() {
        let a = serde_json::json!({ "x": 1 });
        let b = serde_json::json!({ "x": 2 });
        assert_ne!(
            action_hash_for("Bash", &a),
            action_hash_for("Bash", &b)
        );
    }

    // ---- No tool_name ----

    #[test]
    fn missing_tool_name_allows() {
        let fs = MockFs::new();
        let classifier = classifier_with("anything", ReversibilityClass::Catastrophic);
        let auditor = StaticAuditor::pass(0.99);
        let input = HookInput {
            tool_name: None,
            ..Default::default()
        };
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(output.blocked.is_none());
    }
}
