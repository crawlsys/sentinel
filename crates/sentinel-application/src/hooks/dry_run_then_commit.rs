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
//!    and `ReversibleWithEffort` are out of scope (the `tool_usage_gate`
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

use sentinel_domain::dry_run::{AuditorDecision, AuditorError, AuditorVerdict, DryRunRequest};
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
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_value).collect()),
        other => other.clone(),
    }
}

fn approval_marker_path(session_id: &str, action_hash: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{APPROVAL_MARKER_PREFIX}{session_id}-{action_hash}"
    ))
}

/// `true` if the auditor previously approved this exact action in this
/// session.
#[must_use]
pub fn has_dry_run_approval(fs: &dyn FileSystemPort, session_id: &str, action_hash: &str) -> bool {
    fs.exists(&approval_marker_path(session_id, action_hash))
}

/// Record that the auditor approved this action; subsequent identical
/// calls in the same session will short-circuit at step 2 above.
pub fn mark_dry_run_approved(fs: &dyn FileSystemPort, session_id: &str, action_hash: &str) {
    let path = approval_marker_path(session_id, action_hash);
    let _ = fs.write(&path, b"1");
}

/// Returns `true` when the MCP method name (the last `__`-separated segment of
/// `tool_name`, or the `tool` field of a gateway call) is clearly read-only /
/// introspective and therefore cannot have irreversible side-effects.
///
/// This is the **last-resort allow-list** for MCP tools that the
/// `LayeredReversibilityClassifier` has not seen a TOML entry for. When the
/// classifier returns `Irreversible` for an unknown server (the conservative
/// default), this check prevents the A3 gate from demanding
/// `_intent`/`_reasoning`/`_expected_effect` on pure list/get/health calls
/// that carry no data risk.
///
/// The patterns match the method name extracted from the full tool name so
/// that both `mcp__doppler__mcp_list_servers` and a gateway call whose inner
/// tool is `mcp_list_servers` are covered by the same logic.
#[must_use]
pub fn is_readonly_mcp_method(method: &str) -> bool {
    // Exact matches — canonical mcp-router management trio and common introspection names.
    matches!(
        method,
        "mcp_list_servers"
            | "mcp_health_check"
            | "mcp_restart_server" // restart is idempotent / recoverable — RWE at most
            | "health"
            | "status"
            | "ping"
            | "info"
            | "version"
            | "capabilities"
            | "list"
            | "search"
            | "get"
    ) ||
    // Prefix/suffix patterns — anything that looks like read/list/get/health/status/search.
    method.starts_with("list_")
        || method.starts_with("get_")
        || method.starts_with("search_")
        || method.starts_with("find_")
        || method.starts_with("fetch_")
        || method.starts_with("read_")
        || method.starts_with("describe_")
        || method.starts_with("inspect_")
        || method.starts_with("check_")
        || method.ends_with("_list")
        || method.ends_with("_get")
        || method.ends_with("_search")
        || method.ends_with("_status")
        || method.ends_with("_health")
        || method.ends_with("_info")
        || method.ends_with("_ping")
        || method.ends_with("_version")
        || method.contains("_list_")
        || method.contains("_get_")
        || method.contains("_health_")
        || method.contains("_status_")
}

/// Extract the inner tool name from a gateway call's `tool_input`.
///
/// When Claude Code wraps an MCP call via the `mcp_call_tool` gateway, the
/// outer `tool_input` looks like:
/// ```json
/// { "tool": "mcp_list_servers", "arguments": { … } }
/// ```
/// or:
/// ```json
/// { "tool_name": "mcp_list_servers", "arguments": { … } }
/// ```
/// Returns the inner tool name if either key is present and non-empty.
fn gateway_inner_tool(tool_input: &serde_json::Value) -> Option<&str> {
    tool_input
        .get("tool")
        .or_else(|| tool_input.get("tool_name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Resolve the effective MCP method name for the given `(tool_name, tool_input)`.
///
/// For a direct MCP call like `mcp__doppler__mcp_list_servers`, returns
/// `"mcp_list_servers"` (the last `__`-separated segment).
///
/// For a gateway call (`mcp__<gw>__mcp_call_tool` where `tool_input` carries
/// a nested `"tool"` / `"tool_name"` key), returns the inner tool name from
/// the nested field — which is the name that actually determines whether the
/// call is read-only.
fn effective_mcp_method<'a>(tool_name: &'a str, tool_input: &'a serde_json::Value) -> &'a str {
    // Check whether the inner tool field names this as a gateway call first.
    if let Some(inner) = gateway_inner_tool(tool_input) {
        return inner;
    }
    // Fall back to the last segment of the tool name.
    tool_name.rsplit("__").next().unwrap_or(tool_name)
}

/// Unwrap the effective arguments object for dry-run field extraction.
///
/// For a direct call the args are the `tool_input` itself.  For a gateway
/// call the agent must put `_intent`/`_reasoning`/`_expected_effect` inside
/// the nested `"arguments"` object (the only place Claude Code forwards them
/// to the inner MCP tool).  Returns a reference to the nested object when
/// present, otherwise the top-level input.
fn effective_args(tool_input: &serde_json::Value) -> &serde_json::Value {
    // If this is a gateway call and "arguments" is an object, prefer it.
    if gateway_inner_tool(tool_input).is_some() {
        if let Some(args) = tool_input.get("arguments").filter(|v| v.is_object()) {
            return args;
        }
    }
    tool_input
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

    // Step 1b — read-only MCP method short-circuit.
    //
    // The `LayeredReversibilityClassifier` conservatively returns `Irreversible`
    // for any MCP tool not in the TOML config (unknown servers, or known servers
    // missing the specific tool entry). That conservatism is correct for truly
    // unknown mutations, but read-only methods (list_*, get_*, *_health_check,
    // *_list_servers, search, etc.) have zero state-change risk. Demanding
    // `_intent`/`_reasoning`/`_expected_effect` on a list call is a false
    // positive that creates an unbreakable block (the schemas for list/get tools
    // reject unknown parameters, so the fields can never be supplied).
    //
    // Additionally, when the call IS a gateway call (mcp_call_tool wrapping an
    // inner tool), we check the effective method name derived from the nested
    // `"tool"` / `"tool_name"` field.
    if tool.starts_with("mcp__") || tool == "mcp_call_tool" {
        let method = effective_mcp_method(tool, tool_input_ref);
        if is_readonly_mcp_method(method) {
            return HookOutput::allow();
        }
    }

    // Step 1c — Bash tool short-circuit.
    //
    // The Bash tool schema only accepts {command, description, timeout,
    // run_in_background} — there is nowhere to attach `_intent`,
    // `_reasoning`, or `_expected_effect`. Demanding those fields on a
    // Bash-tool git push/commit/merge creates an **unbreakable block**:
    // the agent physically cannot supply them, and `hygiene_override` is
    // unavailable in agent context. This is the same structural problem as
    // the MCP read-only carve-out above (step 1b) — A3 is designed for MCP
    // tool calls that CAN carry prose fields.
    //
    // Shell git operations (git push, git commit, git merge) are already
    // governed by `commit_message_validator`, `pr_merge_gate`,
    // `git_hygiene`, and `db_ops_gate`. A3 does not add safety for them.
    //
    // Safety boundary: `Catastrophic`-class Bash commands (e.g. `rm -rf /`,
    // history rewrites, force-push to a shared branch) still block — but
    // with a message that names the catastrophic-shell reason rather than
    // demanding the impossible dry-run fields. Only `Irreversible`-class
    // Bash (normal git push / commit / merge) is allowed through here.
    if tool == "Bash" {
        if matches!(class, ReversibilityClass::Catastrophic) {
            return HookOutput::deny(format!(
                "🔴 [Dry-Run Auditor] BLOCKED: Catastrophic-class shell command \
                 refused. This command poses unrecoverable risk (e.g. destructive \
                 filesystem removal, git history rewrite, force-push to a shared \
                 branch). If this is intentional and you have explicit human \
                 approval, invoke `hygiene_override` to proceed. (class: {class:?})"
            ));
        }
        // Irreversible-class Bash (git push, git commit, git merge, etc.) — allow.
        // The Bash schema cannot carry A3 dry-run prose fields; demanding them
        // is an unbreakable block. Shell git ops are covered by other sentinel
        // gates; A3 does not apply here.
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

    // Step 4 — auditor scoring. This hook only reaches here for Irreversible
    // / Catastrophic actions, so use the cross-vendor DUAL audit (Opus 4.8 +
    // GPT-5.5, block if either dissents) — a wrong "safe" here is the most
    // expensive error sentinel can make. Auditors without a real second model
    // fall back to single-model via the trait default.
    let verdict = match auditor.score_dual(&dry_run) {
        Ok(v) => v,
        Err(err) => return handle_auditor_error(class, &err),
    };

    // Step 5 — decision.
    decide(fs, session_id, &action_hash, class, &verdict)
}

/// Construct the dry-run request, pulling prose fields out of `tool_input`
/// when the agent supplied them inline (`_intent`, `_reasoning`,
/// `_expected_effect`). Absent fields stay empty; `DryRunRequest::is_complete`
/// is what the caller checks.
///
/// For gateway calls (where `tool_input` contains a nested `"arguments"`
/// object), `effective_args` is used to find the prose fields in the nested
/// scope — the only place the agent can place them when calling through the
/// MCP gateway.
fn build_dry_run(
    session_id: &str,
    tool: &str,
    tool_input: &serde_json::Value,
    class: ReversibilityClass,
    constructed_at: chrono::DateTime<chrono::Utc>,
) -> DryRunRequest {
    // For gateway calls, the _intent/_reasoning/_expected_effect fields
    // live inside the nested "arguments" object, not at the top level.
    let args = effective_args(tool_input);
    let intent = extract_field(args, "_intent");
    let reasoning = extract_field(args, "_reasoning");
    let expected_effect = extract_field(args, "_expected_effect");
    DryRunRequest::new(session_id, tool, tool_input.clone(), class, constructed_at)
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
        // Uses "Edit" (not "Bash") so these generic A3 tests are not affected by
        // the Bash tool carve-out added in step 1c of process(). The carve-out
        // allows Irreversible Bash through immediately, which would short-circuit
        // all the tests that exercise dry-run field validation and auditor scoring.
        HookInput {
            tool_name: Some("Edit".to_string()),
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
        let classifier = classifier_with("Edit", ReversibilityClass::TriviallyReversible);
        let auditor = StaticAuditor::pass(0.99); // shouldn't be consulted
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "Trivially must allow without audit"
        );
    }

    #[test]
    fn reversible_with_effort_actions_bypass_audit() {
        let fs = MockFs::new();
        let classifier = classifier_with("Edit", ReversibilityClass::ReversibleWithEffort);
        let auditor = StaticAuditor::pass(0.99);
        let output = process(&irreversible_input(), &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "RWE handled upstream by tool_usage_gate; A3 silent"
        );
    }

    // ---- Incomplete dry-run ----

    #[test]
    fn missing_intent_field_blocks_with_clear_message() {
        let fs = MockFs::new();
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
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
        assert!(
            reason.contains("_intent"),
            "deny message names the missing field: {reason}"
        );
    }

    #[test]
    fn missing_session_id_blocks() {
        let fs = MockFs::new();
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
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
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.95);
        let input = irreversible_input();
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(output.blocked.is_none());
        // Marker recorded so re-running the same action short-circuits.
        let null = serde_json::Value::Null;
        let tool_input_ref = input.tool_input.as_ref().unwrap_or(&null);
        let hash = action_hash_for("Edit", tool_input_ref);
        assert!(has_dry_run_approval(&fs, "sess-1", &hash));
    }

    #[test]
    fn second_call_with_same_action_short_circuits_without_audit() {
        let fs = MockFs::new();
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
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
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
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
        let classifier = classifier_with("Edit", ReversibilityClass::Catastrophic);
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
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
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
        let classifier = classifier_with("Edit", ReversibilityClass::Irreversible);
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
        let classifier = classifier_with("Edit", ReversibilityClass::Catastrophic);
        let auditor =
            StaticAuditor::err(AuditorError::TimedOut(std::time::Duration::from_secs(30)));
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
        assert_ne!(action_hash_for("Bash", &a), action_hash_for("Bash", &b));
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

    // ---- Bug 1c: Bash tool carve-out ----
    //
    // The Bash tool schema cannot carry `_intent`/`_reasoning`/`_expected_effect`.
    // Demanding those fields on an Irreversible Bash command (git push, git commit,
    // git merge) creates an unbreakable block — the agent cannot supply them, and
    // `hygiene_override` is unavailable in agent context.
    //
    // Fix: Irreversible-class Bash is allowed through (shell git ops are governed
    // by other gates). Catastrophic-class Bash (rm -rf, history rewrite, force-push
    // to shared branch) still blocks — but with a message that names the
    // catastrophic-shell reason rather than demanding the impossible dry-run fields.

    fn bash_git_push_input() -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            session_id: Some("sess-bash".to_string()),
            tool_input: Some(serde_json::json!({
                "command": "git push origin main",
                "description": "push release commit"
                // No _intent/_reasoning/_expected_effect — Bash schema doesn't have them
            })),
            ..Default::default()
        }
    }

    fn bash_catastrophic_input() -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            session_id: Some("sess-bash".to_string()),
            tool_input: Some(serde_json::json!({
                "command": "git push --force origin main",
                "description": "force-push to main (catastrophic)"
            })),
            ..Default::default()
        }
    }

    #[test]
    fn bash_irreversible_git_push_is_allowed_without_dry_run_fields() {
        // Regression test: Bash `git push` classified Irreversible was previously
        // blocked with a demand for _intent/_reasoning/_expected_effect that the
        // Bash schema cannot carry — an unbreakable block in agent context.
        // The fix: Irreversible Bash is allowed through (step 1c carve-out).
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::block("should never be called");
        let output = process(&bash_git_push_input(), &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "Irreversible Bash (git push) must be allowed — Bash schema can't carry dry-run fields"
        );
    }

    #[test]
    fn bash_catastrophic_still_blocks_but_does_not_demand_intent_fields() {
        // Catastrophic Bash (force-push to main, rm -rf, history rewrite) must
        // still be blocked — but the message must NOT demand `_intent`/`_reasoning`/
        // `_expected_effect` (those fields are impossible to supply via Bash schema).
        // Instead the message names the catastrophic-shell reason.
        let fs = MockFs::new();
        let classifier = classifier_with("Bash", ReversibilityClass::Catastrophic);
        let auditor = StaticAuditor::pass(0.99); // should never be reached
        let output = process(&bash_catastrophic_input(), &fs, &classifier, &auditor);
        assert_eq!(
            output.blocked,
            Some(true),
            "Catastrophic Bash must still be blocked"
        );
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        // Must mention catastrophic risk, not demand the impossible dry-run fields.
        assert!(
            reason.contains("Catastrophic") || reason.contains("catastrophic"),
            "block reason should mention catastrophic risk: {reason}"
        );
        assert!(
            !reason.contains("_intent"),
            "block reason must NOT demand _intent fields (Bash schema can't carry them): {reason}"
        );
    }

    #[test]
    fn mcp_irreversible_without_dry_run_fields_still_denied_unchanged() {
        // Verify that the Bash carve-out does NOT affect MCP tool behavior:
        // a mutating MCP tool without dry-run fields must still be blocked.
        // This is the unchanged true-positive path — existing behavior preserved.
        let fs = MockFs::new();
        let classifier = StaticReversibilityClassifier::empty(); // Irreversible default
        let auditor = StaticAuditor::pass(0.99);
        let input = HookInput {
            tool_name: Some("mcp__hyperswitch__create_payment".to_string()),
            session_id: Some("sess-mcp".to_string()),
            tool_input: Some(serde_json::json!({})), // no dry-run fields
            ..Default::default()
        };
        let output = process(&input, &fs, &classifier, &auditor);
        assert_eq!(
            output.blocked,
            Some(true),
            "MCP mutating tool without dry-run fields must still be blocked (unchanged behavior)"
        );
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(
            reason.contains("_intent"),
            "MCP block reason should still demand _intent: {reason}"
        );
    }

    #[test]
    fn mcp_readonly_still_allowed_unchanged() {
        // Verify that MCP read-only carve-out (step 1b) is still in effect:
        // the Bash carve-out must not interfere with existing MCP behavior.
        let fs = MockFs::new();
        let classifier = StaticReversibilityClassifier::empty(); // Irreversible default
        let auditor = StaticAuditor::block("should never be called");
        let input = HookInput {
            tool_name: Some("mcp__doppler__mcp_list_servers".to_string()),
            session_id: Some("sess-mcp".to_string()),
            tool_input: Some(serde_json::json!({})),
            ..Default::default()
        };
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "MCP read-only tool must still be allowed (unchanged behavior)"
        );
    }

    // ---- Bug 1a: read-only MCP method short-circuit ----
    //
    // MCP tools not present in the reversibility TOML default to Irreversible.
    // List/get/health/introspection methods must NOT demand dry-run fields —
    // those schemas reject unknown parameters, creating an unbreakable block.

    fn make_mcp_input(tool_name: &str, extra_input: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool_name.to_string()),
            session_id: Some("sess-1".to_string()),
            tool_input: Some(extra_input),
            ..Default::default()
        }
    }

    /// A classifier that returns Irreversible for all MCP tools — simulates
    /// an unknown server not present in reversibility-defaults.toml.
    fn irreversible_classifier() -> StaticReversibilityClassifier {
        StaticReversibilityClassifier::empty()
    }

    #[test]
    fn readonly_mcp_list_tool_bypasses_audit_even_when_classified_irreversible() {
        // Simulates: mcp__doppler__mcp_list_servers classified as Irreversible
        // (unknown server) — the read-only short-circuit must allow it through
        // without demanding _intent/_reasoning/_expected_effect.
        let fs = MockFs::new();
        // classifier returns Irreversible for everything (conservative default)
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::block("should never be called");
        let input = make_mcp_input(
            "mcp__doppler__mcp_list_servers",
            serde_json::json!({}), // no dry-run fields
        );
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "read-only mcp_list_servers must bypass A3 even when classified Irreversible"
        );
    }

    #[test]
    fn readonly_mcp_health_check_bypasses_audit() {
        let fs = MockFs::new();
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::block("should never be called");
        let input = make_mcp_input(
            "mcp__memory__mcp_health_check",
            serde_json::json!({}),
        );
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "mcp_health_check must bypass A3 without dry-run fields"
        );
    }

    #[test]
    fn readonly_mcp_get_tool_bypasses_audit() {
        let fs = MockFs::new();
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::block("should never be called");
        // list_issues has a list_ prefix -> read-only
        let input = make_mcp_input(
            "mcp__hyperswitch__list_payment_methods",
            serde_json::json!({}),
        );
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "list_* MCP tools must bypass A3 even for unknown servers"
        );
    }

    #[test]
    fn readonly_mcp_search_tool_bypasses_audit() {
        let fs = MockFs::new();
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::block("should never be called");
        let input = make_mcp_input(
            "mcp__memory__memory_search",
            serde_json::json!({}),
        );
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "memory_search (contains _search suffix pattern) must bypass A3"
        );
    }

    #[test]
    fn non_readonly_mcp_tool_still_requires_dry_run_fields() {
        // True-positive check: a mutating MCP tool (unknown server, defaults to
        // Irreversible) must still be blocked when dry-run fields are absent.
        let fs = MockFs::new();
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::pass(0.99); // would pass if reached
        let input = make_mcp_input(
            "mcp__hyperswitch__create_payment",
            serde_json::json!({}), // no dry-run fields
        );
        let output = process(&input, &fs, &classifier, &auditor);
        assert_eq!(
            output.blocked,
            Some(true),
            "mutating MCP tool (create_payment) must still demand dry-run fields"
        );
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap_or("");
        assert!(
            reason.contains("_intent"),
            "block reason should mention missing _intent: {reason}"
        );
    }

    #[test]
    fn non_readonly_mcp_tool_passes_when_dry_run_fields_supplied() {
        // True-positive complement: mutating tool WITH dry-run fields reaches
        // the auditor and passes.
        let fs = MockFs::new();
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::pass(0.95);
        let input = make_mcp_input(
            "mcp__hyperswitch__create_payment",
            serde_json::json!({
                "_intent": "create test payment",
                "_reasoning": "integration test",
                "_expected_effect": "new payment record created"
            }),
        );
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "mutating MCP tool with complete dry-run fields must pass when auditor passes"
        );
    }

    // ---- Bug 1b: gateway tool nested-args unwrapping ----
    //
    // When a gateway call (mcp_call_tool / mcp__<gw>__mcp_call_tool) wraps an
    // inner tool, the agent can only supply _intent/_reasoning/_expected_effect
    // inside the nested "arguments" object — the gateway tool schema does not
    // allow additional top-level fields. The hook must find the fields there.

    #[test]
    fn gateway_call_reads_dry_run_fields_from_nested_arguments() {
        let fs = MockFs::new();
        // Use a classifier that marks the gateway tool as Irreversible to ensure
        // we actually reach the dry-run field extraction step.
        let classifier = classifier_with("mcp__gateway__mcp_call_tool", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.95);
        let input = HookInput {
            tool_name: Some("mcp__gateway__mcp_call_tool".to_string()),
            session_id: Some("sess-gateway".to_string()),
            tool_input: Some(serde_json::json!({
                "tool": "send_payment",  // inner tool — NOT read-only
                "arguments": {
                    "amount": 100,
                    "_intent": "send test payment",
                    "_reasoning": "integration test scenario",
                    "_expected_effect": "payment record created in sandbox"
                }
            })),
            ..Default::default()
        };
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "gateway call with dry-run fields in nested arguments must pass when auditor passes"
        );
    }

    #[test]
    fn gateway_call_fails_when_dry_run_fields_only_at_top_level() {
        // Fields at top-level of a gateway call are NOT visible to the inner tool —
        // the agent must put them in "arguments". This test confirms the hook does
        // NOT look at top-level fields for gateway calls (pre-fix behavior that
        // created the unbreakable block).
        //
        // With the fix: top-level fields ARE checked as fallback. But when the
        // nested "arguments" object is present (even without the fields), the
        // effective_args returns the nested object and the fields are NOT found
        // at the top level.
        let fs = MockFs::new();
        let classifier = classifier_with("mcp__gateway__mcp_call_tool", ReversibilityClass::Irreversible);
        let auditor = StaticAuditor::pass(0.99); // would pass if reached
        let input = HookInput {
            tool_name: Some("mcp__gateway__mcp_call_tool".to_string()),
            session_id: Some("sess-gateway".to_string()),
            tool_input: Some(serde_json::json!({
                "tool": "send_payment",
                "arguments": {
                    "amount": 100
                    // _intent/_reasoning/_expected_effect in arguments — missing
                },
                // Fields at top level won't be found when arguments object present
                "_intent": "I put it at the top level",
                "_reasoning": "incorrect placement",
                "_expected_effect": "this won't be read from here"
            })),
            ..Default::default()
        };
        let output = process(&input, &fs, &classifier, &auditor);
        assert_eq!(
            output.blocked,
            Some(true),
            "gateway call with dry-run fields only at top level (not in arguments) must be blocked"
        );
    }

    #[test]
    fn gateway_call_to_readonly_inner_tool_bypasses_audit() {
        // If the inner tool is read-only (e.g. mcp_list_servers), the whole
        // call should bypass A3 regardless of the gateway tool name.
        let fs = MockFs::new();
        let classifier = irreversible_classifier();
        let auditor = StaticAuditor::block("should never be called");
        let input = HookInput {
            tool_name: Some("mcp__gateway__mcp_call_tool".to_string()),
            session_id: Some("sess-gateway".to_string()),
            tool_input: Some(serde_json::json!({
                "tool": "mcp_list_servers",
                "arguments": {}
            })),
            ..Default::default()
        };
        let output = process(&input, &fs, &classifier, &auditor);
        assert!(
            output.blocked.is_none(),
            "gateway call to read-only inner tool must bypass A3"
        );
    }

    // ---- is_readonly_mcp_method unit tests ----

    #[test]
    fn is_readonly_mcp_method_covers_canonical_names() {
        for method in [
            "mcp_list_servers",
            "mcp_health_check",
            "mcp_restart_server",
            "health",
            "status",
            "ping",
            "info",
            "version",
            "list",
            "search",
            "get",
        ] {
            assert!(
                is_readonly_mcp_method(method),
                "{method} should be classified as read-only"
            );
        }
    }

    #[test]
    fn is_readonly_mcp_method_covers_prefix_patterns() {
        for method in [
            "list_issues",
            "list_servers",
            "get_secret",
            "get_user",
            "search_messages",
            "find_connection",
            "fetch_data",
            "read_file",
            "describe_instance",
            "inspect_resource",
            "check_status",
        ] {
            assert!(
                is_readonly_mcp_method(method),
                "{method} should be classified as read-only via prefix"
            );
        }
    }

    #[test]
    fn is_readonly_mcp_method_covers_suffix_patterns() {
        for method in [
            "tool_list",
            "resource_get",
            "data_search",
            "service_status",
            "endpoint_health",
            "api_info",
            "server_ping",
            "app_version",
        ] {
            assert!(
                is_readonly_mcp_method(method),
                "{method} should be classified as read-only via suffix"
            );
        }
    }

    #[test]
    fn is_readonly_mcp_method_rejects_mutating_names() {
        for method in [
            "create_payment",
            "send_message",
            "delete_resource",
            "update_config",
            "publish",
            "deploy",
            "execute",
            "run",
            "invoke",
            "patch",
            "put",
            "post",
        ] {
            assert!(
                !is_readonly_mcp_method(method),
                "{method} should NOT be classified as read-only"
            );
        }
    }

    // ---- effective_mcp_method + effective_args unit tests ----

    #[test]
    fn effective_mcp_method_returns_last_segment_for_direct_call() {
        let input = serde_json::json!({});
        assert_eq!(
            effective_mcp_method("mcp__doppler__mcp_list_servers", &input),
            "mcp_list_servers"
        );
        assert_eq!(
            effective_mcp_method("mcp__linear__list_issues", &input),
            "list_issues"
        );
    }

    #[test]
    fn effective_mcp_method_returns_inner_tool_for_gateway_call() {
        let input = serde_json::json!({
            "tool": "send_payment",
            "arguments": {}
        });
        assert_eq!(
            effective_mcp_method("mcp__gateway__mcp_call_tool", &input),
            "send_payment"
        );
    }

    #[test]
    fn effective_mcp_method_also_accepts_tool_name_key() {
        let input = serde_json::json!({
            "tool_name": "list_projects",
            "arguments": {}
        });
        assert_eq!(
            effective_mcp_method("mcp__gateway__mcp_call_tool", &input),
            "list_projects"
        );
    }

    #[test]
    fn effective_args_returns_nested_arguments_for_gateway_call() {
        let input = serde_json::json!({
            "tool": "send_payment",
            "arguments": {
                "_intent": "test",
                "amount": 50
            }
        });
        let args = effective_args(&input);
        assert_eq!(args.get("_intent").and_then(|v| v.as_str()), Some("test"));
        assert!(args.get("tool").is_none(), "tool key should not be in effective args");
    }

    #[test]
    fn effective_args_returns_top_level_for_direct_call() {
        let input = serde_json::json!({
            "_intent": "direct intent",
            "_reasoning": "direct reasoning",
            "_expected_effect": "direct effect"
        });
        let args = effective_args(&input);
        assert_eq!(
            args.get("_intent").and_then(|v| v.as_str()),
            Some("direct intent")
        );
    }
}
