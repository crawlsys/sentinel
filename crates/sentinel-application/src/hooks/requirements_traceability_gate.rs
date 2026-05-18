//! BA3 — Requirements-traceability gate (`PreToolUse`).
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §2.3. Structural
//! enforcement of recommendation→requirement traceability: for every
//! [`RequirementRef`] cited in a BA-orchestrator output, validates
//! the reference against the orchestrator's published requirement
//! matrix via [`RequirementMatrixPort`]. Catches BA3 violations at
//! the ``PreToolUse`` layer rather than relying on after-the-fact
//! BA5 critique.
//!
//! Companion to [`provenance_validate`](super::provenance_validate)
//! (BA1). The two hooks share the same trigger pattern (BA output
//! emits structured citations + flags), the same `ValidationMode`
//! shape, and the same operator-ratchet-up rollout posture (start
//! `ObserveOnly` → flip to `DefaultBlocking` → flip to
//! `StrictBlocking` for catastrophic outputs).
//!
//! ## Trigger
//!
//! ``PreToolUse`` for any tool call whose `input.extra` carries either:
//! - `requirement_refs`: a non-empty JSON array of [`RequirementRef`]
//!   entries (the cited matrix rows), and/or
//! - `is_recommendation: true`: a boolean flag the BA-orchestrator
//!   sets when the output is a recommendation (vs. a routine
//!   summary / informational ping / scratchpad). Recommendations
//!   trigger the **Coverage** check; non-recommendations don't.
//!
//! Both fields absent → silent allow (non-BA flow). Phase 4's
//! `config/ba-outputs.toml` will add a tool-name registry so this
//! hook only fires for designated publish tools.
//!
//! ## Four sub-checks per spec §2.3
//!
//! 1. **Existence** — matrix lookup returns `Ok(Some(row))`. If
//!    `Ok(None)` (orchestration tracked but row doesn't exist) →
//!    **Block-class** (recommendation traces to a phantom row).
//!    `Err(UnknownOrchestration)` is also Block — the citation
//!    refers to a non-existent orchestration.
//! 2. **Hash** — cited `content_hash` matches the live matrix row's
//!    hash. Mismatch → warn (the requirement was edited after the
//!    recommendation was authored; operator should confirm the
//!    recommendation still answers the current requirement);
//!    block in `StrictBlocking`.
//! 3. **Coverage** — when `is_recommendation: true`, the output
//!    MUST carry a non-empty `requirement_refs` list. Recommendation
//!    with no traceback → **Block-class**. This is the central BA3
//!    structural violation.
//! 4. **`MatrixStaleness`** — when the matrix endpoint is unreachable
//!    and the adapter is serving from a `last_known_good` snapshot,
//!    the hook surfaces this as a warn-class finding so the
//!    operator sees the staleness. Always warn (matrix availability
//!    isn't catastrophic-bound — the snapshot still validates).

use std::fmt::Write;
use std::time::Duration;

use sentinel_domain::ba::{
    RequirementCheck, RequirementFinding, RequirementRef,
};
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::{RequirementMatrixError, RequirementMatrixPort};

/// Default staleness threshold for the matrix `last_known_good`
/// snapshot. Snapshots older than 24h surface a `MatrixStaleness`
/// finding; operator-configurable in a future phase.
pub const DEFAULT_MATRIX_STALENESS: Duration = Duration::from_secs(24 * 60 * 60);

/// Enforcement mode per spec §3.
///
/// Same shape as
/// [`provenance_validate::ValidationMode`](super::provenance_validate::ValidationMode)
/// so operators rolling up BA1+3 together can use the same posture
/// vocabulary. Currently re-declared rather than re-imported to
/// keep BA1 + BA3 enforcement decoupled at the type level
/// (different mode values per spec are possible in the future).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationMode {
    /// Never blocks. Findings still computed; operator log records
    /// would-be blocks so the rollout can ratchet up.
    ObserveOnly,
    /// Block on Existence + Coverage; warn on Hash + `MatrixStaleness`.
    /// Default for routine BA outputs.
    DefaultBlocking,
    /// Block on Existence + Coverage + Hash; warn on `MatrixStaleness`.
    /// Used for catastrophic-class outputs.
    StrictBlocking,
}

impl ValidationMode {
    /// Returns `true` iff this mode should ever return `deny()`.
    #[must_use]
    pub const fn allows_blocking(self) -> bool {
        !matches!(self, Self::ObserveOnly)
    }

    /// Returns `true` iff Hash findings block in this mode. Only
    /// `StrictBlocking` blocks on hash mismatch.
    #[must_use]
    pub const fn hash_blocks(self) -> bool {
        matches!(self, Self::StrictBlocking)
    }
}

/// Process a ``PreToolUse`` event through the BA3 requirements gate.
///
/// `matrix` is the requirement-matrix lookup port; `mode` drives
/// warn-vs-block.
#[must_use]
pub fn process(
    input: &HookInput,
    matrix: &dyn RequirementMatrixPort,
    mode: ValidationMode,
) -> HookOutput {
    let citations = parse_requirement_refs(input);
    let is_recommendation = is_recommendation(input);

    // Skip silently when neither field signals a BA flow:
    if citations.is_none() && !is_recommendation {
        return HookOutput::allow();
    }

    let citations = citations.unwrap_or_default();
    let mut check = RequirementCheck::passing(citations.clone());
    let mut any_block = false;
    let mut matrix_unavailable_seen = false;

    // (3) Coverage — recommendation must carry at least one
    // requirement_ref. Fires before per-citation checks because
    // it's purely structural.
    if is_recommendation && citations.is_empty() {
        check = check.with_finding(RequirementFinding::Coverage {
            recommendation_summary: extract_recommendation_summary(input),
        });
        any_block = true;
    }

    // Per-citation Existence + Hash checks.
    for citation in &citations {
        match matrix.query_requirement(&citation.orchestration_id, &citation.matrix_row_id) {
            Ok(Some(row)) => {
                if row.content_hash != citation.content_hash {
                    let is_blocking = mode.hash_blocks();
                    check = check.with_finding(RequirementFinding::Hash {
                        orchestration_id: citation.orchestration_id.clone(),
                        matrix_row_id: citation.matrix_row_id.clone(),
                        cited_hash: citation.content_hash.clone(),
                        actual_hash: row.content_hash.clone(),
                    });
                    if is_blocking {
                        any_block = true;
                    }
                }
            }
            Ok(None) | Err(RequirementMatrixError::UnknownOrchestration(_)) => {
                check = check.with_finding(RequirementFinding::Existence {
                    orchestration_id: citation.orchestration_id.clone(),
                    matrix_row_id: citation.matrix_row_id.clone(),
                });
                any_block = true;
            }
            Err(RequirementMatrixError::MatrixUnavailable(msg)) => {
                tracing::warn!(
                    orchestration = %citation.orchestration_id,
                    error = %msg,
                    "requirements_traceability_gate: matrix unavailable (no last_known_good); \
                     soft-warn — citation will not validate but tool not blocked on lookup outage"
                );
                matrix_unavailable_seen = true;
            }
            Err(RequirementMatrixError::Malformed(msg)) => {
                tracing::warn!(
                    orchestration = %citation.orchestration_id,
                    error = %msg,
                    "requirements_traceability_gate: matrix payload malformed; \
                     soft-warn — operator should investigate"
                );
            }
        }
    }

    // (4) MatrixStaleness — surfaced when the adapter is serving
    // from a `last_known_good` snapshot. The port doesn't yet
    // expose snapshot age directly; for Phase 3c we treat any
    // `MatrixUnavailable` as a staleness signal. Always warn.
    if matrix_unavailable_seen {
        check = check.with_finding(RequirementFinding::MatrixStaleness {
            snapshot_age_seconds: DEFAULT_MATRIX_STALENESS.as_secs(),
        });
    }

    if mode.allows_blocking() && any_block {
        HookOutput::deny(format_block_message(&check))
    } else {
        if any_block && !mode.allows_blocking() {
            tracing::warn!(
                citations = check.references.len(),
                "requirements_traceability_gate: ObserveOnly mode suppressed a would-be Block; \
                 flip to DefaultBlocking when telemetry shows the matrix layer is reliable"
            );
        }
        HookOutput::allow()
    }
}

/// Parse `input.extra.requirement_refs` into a
/// `Vec<RequirementRef>`. Returns `None` when the field is absent;
/// returns `Some(Vec::new())` when present but empty (so the
/// Coverage check can fire on `is_recommendation: true` + empty
/// list).
fn parse_requirement_refs(input: &HookInput) -> Option<Vec<RequirementRef>> {
    let value = input.extra.get("requirement_refs")?;
    let array = value.as_array()?;
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        match serde_json::from_value::<RequirementRef>(entry.clone()) {
            Ok(r) => out.push(r),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "requirements_traceability_gate: skipping malformed requirement_ref entry"
                );
            }
        }
    }
    Some(out)
}

/// Read `input.extra.is_recommendation` as a boolean. Defaults to
/// `false`. The BA-orchestrator sets this when emitting a
/// recommendation; non-recommendations bypass the Coverage check.
fn is_recommendation(input: &HookInput) -> bool {
    input
        .extra
        .get("is_recommendation")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Extract a short summary string for the recommendation so the
/// block message names what got flagged. Reads
/// `input.extra.recommendation_summary` (free-form), falling back
/// to the tool name + "(no summary supplied)".
fn extract_recommendation_summary(input: &HookInput) -> String {
    input
        .extra
        .get("recommendation_summary")
        .and_then(|v| v.as_str())
        .map_or_else(
            || {
                let tool = input.tool_name.as_deref().unwrap_or("<unknown tool>");
                format!("{tool} (no summary supplied)")
            },
            ToString::to_string,
        )
}

fn format_block_message(check: &RequirementCheck) -> String {
    let mut buf = String::from(
        "🟠 [BA3 Requirements] BLOCKED: BA output failed structural traceability.\n\n",
    );
    // Render every finding — the mode-aware blocking decision was
    // already made by the caller; `RequirementFinding::is_block()`
    // is the *default-mode* classification (Existence + Coverage),
    // not the mode-adjusted one. Hash mismatches in StrictBlocking
    // contribute to the block decision even though
    // `RequirementFinding::Hash.is_block()` is false.
    for f in &check.findings {
        writeln!(buf, "  {}", describe_finding(f)).ok();
    }
    buf.push_str(
        "\nFix: every recommendation must trace to a stated requirement in the orchestration's \
         matrix. Add the missing `requirement_refs` entry (with the live matrix row's \
         content_hash + statement) and re-issue the publish call. Operator: invoke \
         hygiene_override only for legitimate edge cases — the override is audited.",
    );
    buf
}

fn describe_finding(f: &RequirementFinding) -> String {
    match f {
        RequirementFinding::Existence {
            orchestration_id,
            matrix_row_id,
        } => format!(
            "Existence: matrix row `{matrix_row_id}` in orchestration `{orchestration_id}` does \
             not exist — recommendation traces to a phantom requirement."
        ),
        RequirementFinding::Hash {
            orchestration_id,
            matrix_row_id,
            cited_hash,
            actual_hash,
        } => format!(
            "Hash: row `{matrix_row_id}` in `{orchestration_id}` cites content_hash={cited_hash:?} \
             but the live matrix reports {actual_hash:?}."
        ),
        RequirementFinding::Coverage {
            recommendation_summary,
        } => format!(
            "Coverage: recommendation `{recommendation_summary}` ships with NO requirement_refs \
             — this is the structural BA3 violation."
        ),
        RequirementFinding::MatrixStaleness {
            snapshot_age_seconds,
        } => format!(
            "MatrixStaleness: matrix endpoint unreachable; serving snapshot \
             (~{snapshot_age_seconds}s threshold)."
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Stub `RequirementMatrixPort` backed by a `HashMap` of canned
    /// matrix rows + an optional per-call error queue.
    struct StubMatrix {
        rows: HashMap<(String, String), RequirementRef>,
        next_error: std::sync::Mutex<Option<RequirementMatrixError>>,
    }

    impl StubMatrix {
        fn new() -> Self {
            Self {
                rows: HashMap::new(),
                next_error: std::sync::Mutex::new(None),
            }
        }

        fn with(mut self, row: RequirementRef) -> Self {
            self.rows.insert(
                (row.orchestration_id.clone(), row.matrix_row_id.clone()),
                row,
            );
            self
        }

        fn with_next_error(self, err: RequirementMatrixError) -> Self {
            *self.next_error.lock().unwrap() = Some(err);
            self
        }
    }

    impl RequirementMatrixPort for StubMatrix {
        fn query_requirement(
            &self,
            orchestration_id: &str,
            matrix_row_id: &str,
        ) -> Result<Option<RequirementRef>, RequirementMatrixError> {
            let next = self.next_error.lock().unwrap().take();
            if let Some(err) = next {
                return Err(err);
            }
            Ok(self
                .rows
                .get(&(orchestration_id.to_string(), matrix_row_id.to_string()))
                .cloned())
        }
    }

    fn req(orchestration: &str, row: &str, hash: &str, statement: &str) -> RequirementRef {
        RequirementRef {
            orchestration_id: orchestration.to_string(),
            matrix_row_id: row.to_string(),
            content_hash: hash.to_string(),
            statement: statement.to_string(),
        }
    }

    fn input_with(extras: Vec<(&str, serde_json::Value)>) -> HookInput {
        let mut extra = serde_json::Map::new();
        for (k, v) in extras {
            extra.insert(k.to_string(), v);
        }
        HookInput {
            tool_name: Some("ba_orchestrator__publish".to_string()),
            session_id: Some("s1".to_string()),
            extra,
            ..Default::default()
        }
    }

    fn refs_json(refs: Vec<RequirementRef>) -> serde_json::Value {
        serde_json::to_value(refs).unwrap()
    }

    fn deny_reason(output: &HookOutput) -> String {
        output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
            .unwrap_or_default()
    }

    // ---- Skip paths ----

    #[test]
    fn allows_when_neither_field_present() {
        let matrix = StubMatrix::new();
        let input = HookInput::default();
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn allows_when_is_recommendation_false_and_no_refs() {
        let matrix = StubMatrix::new();
        let input = input_with(vec![("is_recommendation", serde_json::json!(false))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn allows_when_refs_present_and_match() {
        let row = req("case-1", "R-001", "hash-v1", "Stakeholder needs churn under 2%");
        let matrix = StubMatrix::new().with(row.clone());
        let input = input_with(vec![("requirement_refs", refs_json(vec![row]))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    // ---- Coverage — central BA3 violation ----

    #[test]
    fn blocks_when_recommendation_has_no_refs() {
        let matrix = StubMatrix::new();
        let input = input_with(vec![
            ("is_recommendation", serde_json::json!(true)),
            ("recommendation_summary", serde_json::json!("Raise prices by 8%")),
        ]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, Some(true));
        let reason = deny_reason(&output);
        assert!(reason.contains("Coverage"));
        assert!(reason.contains("Raise prices"));
    }

    #[test]
    fn coverage_block_uses_tool_name_fallback_summary() {
        let matrix = StubMatrix::new();
        let input = input_with(vec![("is_recommendation", serde_json::json!(true))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        let reason = deny_reason(&output);
        assert!(reason.contains("ba_orchestrator__publish"));
        assert!(reason.contains("no summary supplied"));
    }

    #[test]
    fn coverage_does_not_fire_when_not_a_recommendation() {
        // Empty requirement_refs but is_recommendation=false →
        // legitimate non-recommendation BA output; allow.
        let matrix = StubMatrix::new();
        let input = input_with(vec![
            ("is_recommendation", serde_json::json!(false)),
            ("requirement_refs", refs_json(vec![])),
        ]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    // ---- Existence — phantom requirement ----

    #[test]
    fn blocks_on_phantom_row() {
        let matrix = StubMatrix::new(); // No rows registered
        let phantom = req("case-1", "R-PHANTOM", "h", "Some claim");
        let input = input_with(vec![("requirement_refs", refs_json(vec![phantom]))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, Some(true));
        assert!(deny_reason(&output).contains("Existence"));
    }

    #[test]
    fn blocks_on_unknown_orchestration() {
        let phantom = req("unknown-case", "R-001", "h", "x");
        let matrix = StubMatrix::new().with_next_error(
            RequirementMatrixError::UnknownOrchestration("unknown-case".to_string()),
        );
        let input = input_with(vec![("requirement_refs", refs_json(vec![phantom]))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, Some(true));
        let reason = deny_reason(&output);
        assert!(
            reason.contains("Existence"),
            "UnknownOrchestration must map to an Existence block; got: {reason}"
        );
    }

    // ---- Hash — warn in default, block in strict ----

    #[test]
    fn hash_mismatch_warns_in_default_mode() {
        let live = req("case-1", "R-001", "new-hash", "current statement");
        let cited = req("case-1", "R-001", "old-hash", "stale statement");
        let matrix = StubMatrix::new().with(live);
        let input = input_with(vec![("requirement_refs", refs_json(vec![cited]))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None, "Default mode warns on hash mismatch");
    }

    #[test]
    fn hash_mismatch_blocks_in_strict_mode() {
        let live = req("case-1", "R-001", "new-hash", "current");
        let cited = req("case-1", "R-001", "old-hash", "stale");
        let matrix = StubMatrix::new().with(live);
        let input = input_with(vec![("requirement_refs", refs_json(vec![cited]))]);
        let output = process(&input, &matrix, ValidationMode::StrictBlocking);
        assert_eq!(output.blocked, Some(true));
        let reason = deny_reason(&output);
        assert!(reason.contains("Hash"));
    }

    // ---- MatrixUnavailable — soft-warn + staleness ----

    #[test]
    fn matrix_unavailable_does_not_block_but_logs() {
        let cited = req("case-1", "R-001", "h", "x");
        let matrix = StubMatrix::new().with_next_error(
            RequirementMatrixError::MatrixUnavailable("timeout".to_string()),
        );
        let input = input_with(vec![("requirement_refs", refs_json(vec![cited]))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(
            output.blocked, None,
            "MatrixUnavailable is soft-warn — operator log only, never block"
        );
    }

    #[test]
    fn malformed_matrix_response_does_not_block() {
        let cited = req("case-1", "R-001", "h", "x");
        let matrix = StubMatrix::new().with_next_error(
            RequirementMatrixError::Malformed("schema mismatch".to_string()),
        );
        let input = input_with(vec![("requirement_refs", refs_json(vec![cited]))]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    // ---- ObserveOnly suppresses everything ----

    #[test]
    fn observe_only_never_blocks_even_on_phantom_row() {
        let matrix = StubMatrix::new();
        let phantom = req("case-1", "R-PHANTOM", "h", "x");
        let input = input_with(vec![("requirement_refs", refs_json(vec![phantom]))]);
        let output = process(&input, &matrix, ValidationMode::ObserveOnly);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn observe_only_never_blocks_even_on_coverage_failure() {
        let matrix = StubMatrix::new();
        let input = input_with(vec![("is_recommendation", serde_json::json!(true))]);
        let output = process(&input, &matrix, ValidationMode::ObserveOnly);
        assert_eq!(output.blocked, None);
    }

    // ---- Mode predicates ----

    #[test]
    fn mode_allows_blocking_only_for_blocking_modes() {
        assert!(!ValidationMode::ObserveOnly.allows_blocking());
        assert!(ValidationMode::DefaultBlocking.allows_blocking());
        assert!(ValidationMode::StrictBlocking.allows_blocking());
    }

    #[test]
    fn mode_hash_blocks_only_in_strict() {
        assert!(!ValidationMode::ObserveOnly.hash_blocks());
        assert!(!ValidationMode::DefaultBlocking.hash_blocks());
        assert!(ValidationMode::StrictBlocking.hash_blocks());
    }

    // ---- Aggregate block message ----

    #[test]
    fn block_reason_lists_every_failing_finding() {
        let matrix = StubMatrix::new();
        let phantom1 = req("case-1", "R-A", "h", "x");
        let phantom2 = req("case-1", "R-B", "h", "y");
        let input = input_with(vec![
            ("is_recommendation", serde_json::json!(true)),
            ("recommendation_summary", serde_json::json!("Adopt SaaS expansion")),
            ("requirement_refs", refs_json(vec![phantom1, phantom2])),
        ]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        let reason = deny_reason(&output);
        // Both Existence findings should be named in the message
        assert!(reason.contains("R-A"));
        assert!(reason.contains("R-B"));
        // Recommendation is non-empty so Coverage doesn't fire (the
        // phantom existence findings are the structural failure).
    }

    #[test]
    fn malformed_requirement_ref_entries_are_skipped_individually() {
        // One well-formed + one malformed → only the well-formed is
        // validated. The malformed one is logged + skipped.
        let row = req("case-1", "R-001", "h", "x");
        let matrix = StubMatrix::new().with(row.clone());
        let refs = serde_json::json!([
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({"bogus": "shape"}),
        ]);
        let input = input_with(vec![("requirement_refs", refs)]);
        let output = process(&input, &matrix, ValidationMode::DefaultBlocking);
        // Well-formed citation validates against the matrix row;
        // malformed entry is silently skipped → overall allow.
        assert_eq!(output.blocked, None);
    }
}
