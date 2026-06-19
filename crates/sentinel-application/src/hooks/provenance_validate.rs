//! BA1 — Provenance-validate hook (`PreToolUse`).
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §2.2. Structural
//! enforcement of citation provenance: for every
//! [`ArtifactReference`] in a BA-orchestrator output, validates the
//! citation against the connector-audit chain built by
//! [`audit_extract`](super::audit_extract). Catches BA1 violations
//! at the `PreToolUse` layer rather than relying on after-the-fact
//! BA5 critique.
//!
//! ## Trigger
//!
//! `PreToolUse` for any tool call whose `input.extra.artifacts` field
//! is a non-empty JSON array. The presence of the field is the
//! signal that the caller is asserting citations — if it's absent
//! the hook silently allows (non-BA tools don't go through this
//! check). Phase 4's `config/ba-outputs.toml` will additionally
//! gate by tool name; for Phase 3b the field-presence trigger is
//! sufficient to validate BA flows that opt in.
//!
//! ## Four sub-checks per spec §2.2
//!
//! 1. **Existence** — at least one [`RetrievalRecord`] exists for
//!    the cited `artifact_id`. Always Block-class — citation is
//!    unverifiable without it.
//! 2. **Freshness** — the cited `content_hash` matches the
//!    most-recent record's hash. Block in
//!    [`ValidationMode::StrictBlocking`], warn otherwise (per
//!    spec §3 recommendation: block for catastrophic-class
//!    outputs, warn for routine).
//! 3. **`ProvenanceClass`** — the cited class matches what the
//!    connector reported. Always warn — catches mis-classification
//!    without blocking (operator may have legitimately reclassified).
//! 4. **`WithinSession`** — the most-recent record's session matches
//!    the current session, OR its timestamp is within the lookback
//!    window (default 24h). Block in `StrictBlocking`, warn
//!    otherwise.
//!
//! ## Modes
//!
//! [`ValidationMode`] drives the warn-vs-block decisions per spec §3.
//! Operators ratchet up during rollout: start in `ObserveOnly`
//! (never block, only log) → flip to `DefaultBlocking` once
//! confident the connector layer reliably emits audit events →
//! flip to `StrictBlocking` for catastrophic-class output tools.

use std::fmt::Write;
use std::time::Duration;

use chrono::Utc;
#[cfg(test)]
use sentinel_domain::ba::ProvenanceClass;
use sentinel_domain::ba::{ArtifactReference, ProvenanceCheck, ProvenanceFinding, RetrievalRecord};
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::ProvenancePort;

use super::concrete_input_session_id;

/// Default cross-session lookback window per spec §2.2: a retrieval
/// older than 24h fails the `WithinSession` check.
pub const DEFAULT_LOOKBACK: Duration = Duration::from_secs(24 * 60 * 60);

/// Enforcement mode per spec §3. Drives warn-vs-block decisions on
/// non-Existence findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationMode {
    /// Never blocks. Every check still runs and findings are
    /// computed, but the hook returns `allow()` regardless. Used
    /// during rollout to gather telemetry on what WOULD be blocked
    /// before flipping to enforcement.
    ObserveOnly,
    /// Block on `Existence`; warn on Freshness/ProvenanceClass/
    /// `WithinSession`. Default for routine BA outputs.
    DefaultBlocking,
    /// Block on `Existence` + `Freshness` + `WithinSession`; warn
    /// on `ProvenanceClass`. Used for catastrophic-class outputs
    /// (board decks, signed customer briefs, regulatory filings).
    StrictBlocking,
}

impl ValidationMode {
    /// Returns `true` iff this mode should ever return `deny()`.
    /// Always-false for `ObserveOnly`; true for both blocking
    /// variants.
    #[must_use]
    pub const fn allows_blocking(self) -> bool {
        !matches!(self, Self::ObserveOnly)
    }

    /// Returns `true` iff Freshness findings should block in this
    /// mode. Only `StrictBlocking` blocks on freshness.
    #[must_use]
    pub const fn freshness_blocks(self) -> bool {
        matches!(self, Self::StrictBlocking)
    }

    /// Returns `true` iff `WithinSession` findings should block in
    /// this mode. Only `StrictBlocking` blocks on stale lookback.
    #[must_use]
    pub const fn within_session_blocks(self) -> bool {
        matches!(self, Self::StrictBlocking)
    }
}

#[derive(Debug, Clone)]
pub struct ProvenanceValidationEvaluation {
    pub checks: Vec<ProvenanceCheck>,
    pub mode: ValidationMode,
    pub would_block: bool,
    pub should_block: bool,
}

#[must_use]
pub fn is_ba1_signal(input: &HookInput) -> bool {
    parse_citations(input).is_some()
}

/// Process a ``PreToolUse`` event through the BA1 provenance gate.
///
/// `provenance` reads the audit chain; `mode` drives warn-vs-block.
/// The lookback window for `WithinSession` defaults to 24h; callers
/// needing a different window use [`process_with_lookback`].
#[must_use]
pub fn process(
    input: &HookInput,
    provenance: &dyn ProvenancePort,
    mode: ValidationMode,
) -> HookOutput {
    process_with_lookback(input, provenance, mode, DEFAULT_LOOKBACK)
}

/// [`process`] with an explicit lookback window.
#[must_use]
pub fn process_with_lookback(
    input: &HookInput,
    provenance: &dyn ProvenancePort,
    mode: ValidationMode,
    lookback: Duration,
) -> HookOutput {
    let Some(evaluation) = evaluate_with_lookback(input, provenance, mode, lookback) else {
        return HookOutput::allow();
    };
    output_from_evaluation(&evaluation)
}

#[must_use]
pub fn evaluate(
    input: &HookInput,
    provenance: &dyn ProvenancePort,
    mode: ValidationMode,
) -> Option<ProvenanceValidationEvaluation> {
    evaluate_with_lookback(input, provenance, mode, DEFAULT_LOOKBACK)
}

#[must_use]
pub fn evaluate_with_lookback(
    input: &HookInput,
    provenance: &dyn ProvenancePort,
    mode: ValidationMode,
    lookback: Duration,
) -> Option<ProvenanceValidationEvaluation> {
    let Some(citations) = parse_citations(input) else {
        // Either no `artifacts` field at all, or it's empty / malformed.
        // BA flows that don't assert citations go through here unchanged.
        return None;
    };

    let session_id = concrete_input_session_id(input);
    let now = Utc::now();
    let lookback_cutoff =
        now - chrono::Duration::from_std(lookback).unwrap_or_else(|_| chrono::Duration::hours(24));

    let mut checks: Vec<ProvenanceCheck> = Vec::with_capacity(citations.len());
    let mut would_block = false;

    for citation in citations {
        let history = provenance.query_artifact_history(&citation.artifact_id);
        let history = match history {
            Ok(h) => h,
            Err(err) => {
                tracing::warn!(
                    artifact = %citation.artifact_id,
                    error = %err,
                    "provenance_validate: port query failed; blocking because citation cannot be validated"
                );
                let finding = match err {
                    sentinel_domain::ports::ProvenanceError::StoreUnavailable(reason) => {
                        ProvenanceFinding::StoreUnavailable {
                            artifact_id: citation.artifact_id.clone(),
                            reason,
                        }
                    }
                    sentinel_domain::ports::ProvenanceError::Malformed(reason) => {
                        ProvenanceFinding::StoreMalformed {
                            artifact_id: citation.artifact_id.clone(),
                            reason,
                        }
                    }
                };
                let check = ProvenanceCheck::passing(citation).with_finding(finding);
                would_block |= check.has_block();
                checks.push(check);
                continue;
            }
        };
        let mut check = ProvenanceCheck::passing(citation.clone());

        // (1) Existence — always Block-class.
        let Some(latest) = pick_latest(&history) else {
            check = check.with_finding(ProvenanceFinding::Existence {
                artifact_id: citation.artifact_id.clone(),
            });
            would_block |= check.has_block();
            checks.push(check);
            continue;
        };

        // (2) Freshness — Block in StrictBlocking, warn otherwise.
        if latest.content_hash != citation.content_hash {
            let is_blocking = mode.freshness_blocks();
            check = check.with_finding(ProvenanceFinding::Freshness {
                artifact_id: citation.artifact_id.clone(),
                cited_hash: citation.content_hash.clone(),
                actual_hash: latest.content_hash.clone(),
                is_blocking,
            });
            if is_blocking {
                would_block = true;
            }
        }

        // (3) ProvenanceClass — always warn.
        if latest.provenance_class != citation.provenance_class {
            check = check.with_finding(ProvenanceFinding::ProvenanceClass {
                artifact_id: citation.artifact_id.clone(),
                cited_class: citation.provenance_class,
                actual_class: latest.provenance_class,
            });
        }

        // (4) WithinSession — Block in StrictBlocking, warn otherwise.
        if session_id != Some(latest.session_id.as_str()) && latest.retrieved_at < lookback_cutoff {
            let is_blocking = mode.within_session_blocks();
            check = check.with_finding(ProvenanceFinding::WithinSession {
                artifact_id: citation.artifact_id.clone(),
                retrieved_at: latest.retrieved_at,
                cutoff: lookback_cutoff,
                is_blocking,
            });
            if is_blocking {
                would_block = true;
            }
        }

        checks.push(check);
    }

    let should_block = mode.allows_blocking() && would_block;
    Some(ProvenanceValidationEvaluation {
        checks,
        mode,
        would_block,
        should_block,
    })
}

#[must_use]
pub fn output_from_evaluation(evaluation: &ProvenanceValidationEvaluation) -> HookOutput {
    if evaluation.should_block {
        HookOutput::deny(format_block_message(&evaluation.checks))
    } else {
        if evaluation.would_block && !evaluation.mode.allows_blocking() {
            // ObserveOnly: would have blocked, but mode forbids it.
            // Surface in operator log for ratchet-up calibration.
            tracing::warn!(
                citations = evaluation.checks.len(),
                "provenance_validate: ObserveOnly mode suppressed a would-be Block; \
                 flip to DefaultBlocking when telemetry shows the connector layer \
                 reliably emits audit events"
            );
        }
        HookOutput::allow()
    }
}

/// Parse `input.extra.artifacts` into a `Vec<ArtifactReference>`.
/// Returns `None` when the field is absent, empty, or malformed —
/// callers treat all three as "this isn't a BA flow, skip silently."
fn parse_citations(input: &HookInput) -> Option<Vec<ArtifactReference>> {
    let value = input.extra.get("artifacts")?;
    let array = value.as_array()?;
    if array.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        match serde_json::from_value::<ArtifactReference>(entry.clone()) {
            Ok(r) => out.push(r),
            Err(err) => {
                tracing::warn!(error = %err, "provenance_validate: skipping malformed citation entry");
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Pick the most-recent record from the history. Returns `None`
/// when history is empty.
fn pick_latest(history: &[RetrievalRecord]) -> Option<&RetrievalRecord> {
    history.iter().max_by_key(|r| r.retrieved_at)
}

/// Render the block message naming each failing citation + its
/// finding(s). Carried verbatim as the deny reason so the agent
/// sees a precise diagnostic.
fn format_block_message(checks: &[ProvenanceCheck]) -> String {
    let mut buf = String::from(
        "🟠 [BA1 Provenance] BLOCKED: one or more citations failed structural validation.\n\n",
    );
    for check in checks {
        if !check.has_block() {
            continue;
        }
        writeln!(buf, "- {}", check.citation.artifact_id).ok();
        for f in &check.findings {
            if !f.is_block() {
                continue;
            }
            writeln!(buf, "    {}", describe_finding(f)).ok();
        }
    }
    buf.push_str(
        "\nFix: ensure the cited artifacts were retrieved via a registered MCP connector \
         IN THIS SESSION (or within the configured lookback). Re-issue the publish call after \
         the connector has fetched the artifact + emitted its audit event. Operator: invoke \
         hygiene_override only for legitimate edge cases — the override is audited.",
    );
    buf
}

fn describe_finding(f: &ProvenanceFinding) -> String {
    match f {
        ProvenanceFinding::Existence { artifact_id } => format!(
            "Existence: no retrieval record exists for `{artifact_id}` — the connector was \
             never called for this artifact in the audit window."
        ),
        ProvenanceFinding::Freshness {
            artifact_id,
            cited_hash,
            actual_hash,
            ..
        } => format!(
            "Freshness: `{artifact_id}` cites content_hash={cited_hash:?} but the latest \
             retrieval reports {actual_hash:?}."
        ),
        ProvenanceFinding::ProvenanceClass {
            artifact_id,
            cited_class,
            actual_class,
        } => format!(
            "ProvenanceClass: `{artifact_id}` cites {cited_class:?} but the connector reported \
             {actual_class:?}."
        ),
        ProvenanceFinding::WithinSession {
            artifact_id,
            retrieved_at,
            cutoff,
            ..
        } => format!(
            "WithinSession: `{artifact_id}` last retrieved at {retrieved_at} is older than the \
             lookback cutoff {cutoff} and not from the current session."
        ),
        ProvenanceFinding::StoreUnavailable {
            artifact_id,
            reason,
        } => format!(
            "StoreUnavailable: provenance store could not be read for `{artifact_id}` \
             ({reason}); citation cannot be validated."
        ),
        ProvenanceFinding::StoreMalformed {
            artifact_id,
            reason,
        } => format!(
            "StoreMalformed: provenance store returned malformed data for `{artifact_id}` \
             ({reason}); citation cannot be validated."
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use chrono::{DateTime, Utc};
    use sentinel_domain::ports::ProvenanceError;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Stub `ProvenancePort` backed by a `HashMap` of canned histories.
    struct StubPort {
        histories: HashMap<String, Vec<RetrievalRecord>>,
        next_error: Mutex<Option<ProvenanceError>>,
    }

    impl StubPort {
        fn new() -> Self {
            Self {
                histories: HashMap::new(),
                next_error: Mutex::new(None),
            }
        }

        fn with(mut self, artifact_id: &str, records: Vec<RetrievalRecord>) -> Self {
            self.histories.insert(artifact_id.to_string(), records);
            self
        }

        fn with_next_error(self, err: ProvenanceError) -> Self {
            *self.next_error.lock().unwrap() = Some(err);
            self
        }
    }

    impl ProvenancePort for StubPort {
        fn query_artifact_history(
            &self,
            artifact_id: &str,
        ) -> Result<Vec<RetrievalRecord>, ProvenanceError> {
            let next = self.next_error.lock().unwrap().take();
            if let Some(err) = next {
                return Err(err);
            }
            Ok(self.histories.get(artifact_id).cloned().unwrap_or_default())
        }
    }

    fn deny_reason(output: &HookOutput) -> String {
        output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
            .unwrap_or_default()
    }

    fn record(
        artifact_id: &str,
        content_hash: &str,
        class: ProvenanceClass,
        session_id: &str,
        retrieved_at: DateTime<Utc>,
    ) -> RetrievalRecord {
        RetrievalRecord {
            artifact_id: artifact_id.to_string(),
            connector_name: "mcp__linear__get_issue".to_string(),
            content_hash: content_hash.to_string(),
            provenance_class: class,
            session_id: session_id.to_string(),
            retrieved_at,
        }
    }

    fn citation(
        artifact_id: &str,
        content_hash: &str,
        class: ProvenanceClass,
    ) -> ArtifactReference {
        ArtifactReference {
            artifact_id: artifact_id.to_string(),
            content_hash: content_hash.to_string(),
            provenance_class: class,
            retrieved_at: Utc::now(),
        }
    }

    fn input_with_citations(session: &str, citations: Vec<ArtifactReference>) -> HookInput {
        let json = serde_json::to_value(citations).unwrap();
        let mut extra = serde_json::Map::new();
        extra.insert("artifacts".to_string(), json);
        HookInput {
            tool_name: Some("ba_orchestrator__publish".to_string()),
            session_id: Some(session.to_string()),
            extra,
            ..Default::default()
        }
    }

    // ---- Skip paths ----

    #[test]
    fn allows_when_no_artifacts_field() {
        let port = StubPort::new();
        let input = HookInput::default();
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn allows_when_artifacts_array_empty() {
        let port = StubPort::new();
        let mut extra = serde_json::Map::new();
        extra.insert("artifacts".to_string(), serde_json::json!([]));
        let input = HookInput {
            extra,
            ..Default::default()
        };
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn allows_when_artifacts_malformed() {
        let port = StubPort::new();
        let mut extra = serde_json::Map::new();
        // Not an array
        extra.insert("artifacts".to_string(), serde_json::json!("oops"));
        let input = HookInput {
            extra,
            ..Default::default()
        };
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn blocks_when_store_unavailable() {
        let port =
            StubPort::new().with_next_error(ProvenanceError::StoreUnavailable("disk full".into()));
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, Some(true));
        assert!(deny_reason(&output).contains("StoreUnavailable"));
    }

    #[test]
    fn blocks_when_store_malformed() {
        let port = StubPort::new().with_next_error(ProvenanceError::Malformed("bad json".into()));
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, Some(true));
        assert!(deny_reason(&output).contains("StoreMalformed"));
    }

    // ---- Happy path ----

    #[test]
    fn allows_when_all_citations_validate() {
        let now = Utc::now();
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "h1",
                ProvenanceClass::SystemOfRecord,
                "s1",
                now,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None);
    }

    // ---- Existence (always blocks in any blocking mode) ----

    #[test]
    fn blocks_on_missing_history_in_default_mode() {
        let port = StubPort::new(); // No history for FIR-1
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
            .unwrap_or_default();
        assert!(reason.contains("Existence"));
        assert!(reason.contains("FIR-1"));
    }

    #[test]
    fn blocks_on_missing_history_in_strict_mode() {
        let port = StubPort::new();
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::StrictBlocking);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn observe_only_never_blocks_even_on_existence_failure() {
        let port = StubPort::new();
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::ObserveOnly);
        assert_eq!(
            output.blocked, None,
            "ObserveOnly suppresses Block findings — telemetry only"
        );
    }

    // ---- Freshness (warns in Default, blocks in Strict) ----

    #[test]
    fn freshness_mismatch_warns_in_default_mode() {
        let now = Utc::now();
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "actual-hash",
                ProvenanceClass::SystemOfRecord,
                "s1",
                now,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![citation(
                "FIR-1",
                "stale-hash",
                ProvenanceClass::SystemOfRecord,
            )],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(
            output.blocked, None,
            "Default mode warns on freshness, doesn't block"
        );
    }

    #[test]
    fn freshness_mismatch_blocks_in_strict_mode() {
        let now = Utc::now();
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "actual-hash",
                ProvenanceClass::SystemOfRecord,
                "s1",
                now,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![citation(
                "FIR-1",
                "stale-hash",
                ProvenanceClass::SystemOfRecord,
            )],
        );
        let output = process(&input, &port, ValidationMode::StrictBlocking);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
            .unwrap_or_default();
        assert!(reason.contains("Freshness"));
    }

    // ---- ProvenanceClass (always warns) ----

    #[test]
    fn class_mismatch_warns_in_strict_mode() {
        // Strict still doesn't block on class mismatch per spec.
        let now = Utc::now();
        let port = StubPort::new().with(
            "FIR-1",
            vec![record("FIR-1", "h1", ProvenanceClass::Inference, "s1", now)],
        );
        // Citation claims SystemOfRecord but connector said Inference.
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::StrictBlocking);
        assert_eq!(
            output.blocked, None,
            "ProvenanceClass mismatch is always warn (operator may have legitimately reclassified)"
        );
    }

    // ---- WithinSession (warns in Default, blocks in Strict) ----

    #[test]
    fn within_session_stale_warns_in_default_mode() {
        let two_days_ago = Utc::now() - ChronoDuration::hours(48);
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "h1",
                ProvenanceClass::SystemOfRecord,
                "older-session",
                two_days_ago,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(output.blocked, None, "Default mode warns on stale lookback");
    }

    #[test]
    fn within_session_stale_blocks_in_strict_mode() {
        let two_days_ago = Utc::now() - ChronoDuration::hours(48);
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "h1",
                ProvenanceClass::SystemOfRecord,
                "older-session",
                two_days_ago,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::StrictBlocking);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
            .unwrap_or_default();
        assert!(reason.contains("WithinSession"));
    }

    #[test]
    fn missing_session_does_not_match_empty_retrieval_session() {
        let two_days_ago = Utc::now() - ChronoDuration::hours(48);
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "h1",
                ProvenanceClass::SystemOfRecord,
                "",
                two_days_ago,
            )],
        );
        let json = serde_json::to_value(vec![citation(
            "FIR-1",
            "h1",
            ProvenanceClass::SystemOfRecord,
        )])
        .unwrap();
        let mut extra = serde_json::Map::new();
        extra.insert("artifacts".to_string(), json);
        let input = HookInput {
            extra,
            ..Default::default()
        };

        let output = process(&input, &port, ValidationMode::StrictBlocking);

        assert_eq!(output.blocked, Some(true));
        assert!(deny_reason(&output).contains("WithinSession"));
    }

    #[test]
    fn synthetic_unknown_session_does_not_match_unknown_retrieval_session() {
        let two_days_ago = Utc::now() - ChronoDuration::hours(48);
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "h1",
                ProvenanceClass::SystemOfRecord,
                "unknown",
                two_days_ago,
            )],
        );
        let input = input_with_citations(
            " unknown ",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );

        let output = process(&input, &port, ValidationMode::StrictBlocking);

        assert_eq!(output.blocked, Some(true));
        assert!(deny_reason(&output).contains("WithinSession"));
    }

    #[test]
    fn within_session_recent_passes_even_with_different_session() {
        // Different session but within the 24h lookback — that's the
        // documented allow-with-warn behavior. We use a 1h-ago
        // timestamp to keep it well within the default lookback.
        let one_hour_ago = Utc::now() - ChronoDuration::hours(1);
        let port = StubPort::new().with(
            "FIR-1",
            vec![record(
                "FIR-1",
                "h1",
                ProvenanceClass::SystemOfRecord,
                "different-session",
                one_hour_ago,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![citation("FIR-1", "h1", ProvenanceClass::SystemOfRecord)],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        assert_eq!(
            output.blocked, None,
            "different session within 24h lookback is acceptable"
        );
    }

    // ---- Aggregate behaviour ----

    #[test]
    fn block_reason_lists_every_failing_citation() {
        // Two missing citations + one valid → block names both
        // failures.
        let now = Utc::now();
        let port = StubPort::new().with(
            "FIR-OK",
            vec![record(
                "FIR-OK",
                "h-ok",
                ProvenanceClass::SystemOfRecord,
                "s1",
                now,
            )],
        );
        let input = input_with_citations(
            "s1",
            vec![
                citation("FIR-OK", "h-ok", ProvenanceClass::SystemOfRecord),
                citation("FIR-MISSING-1", "h", ProvenanceClass::SystemOfRecord),
                citation("FIR-MISSING-2", "h", ProvenanceClass::SystemOfRecord),
            ],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
            .unwrap_or_default();
        assert!(reason.contains("FIR-MISSING-1"));
        assert!(reason.contains("FIR-MISSING-2"));
        // FIR-OK passes — shouldn't appear in the block list (but the
        // message format is a free-text render so we don't assert
        // absence too strictly).
    }

    #[test]
    fn picks_latest_record_when_multiple_exist() {
        // History has two records for same artifact. Validation
        // should match against the LATER record's hash.
        let old = Utc::now() - ChronoDuration::hours(2);
        let recent = Utc::now();
        let port = StubPort::new().with(
            "FIR-1",
            vec![
                record(
                    "FIR-1",
                    "old-hash",
                    ProvenanceClass::SystemOfRecord,
                    "s1",
                    old,
                ),
                record(
                    "FIR-1",
                    "new-hash",
                    ProvenanceClass::SystemOfRecord,
                    "s1",
                    recent,
                ),
            ],
        );
        // Citation matches the NEW (latest) hash → should validate.
        let input = input_with_citations(
            "s1",
            vec![citation(
                "FIR-1",
                "new-hash",
                ProvenanceClass::SystemOfRecord,
            )],
        );
        let output = process(&input, &port, ValidationMode::DefaultBlocking);
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
    fn mode_freshness_blocks_only_in_strict() {
        assert!(!ValidationMode::ObserveOnly.freshness_blocks());
        assert!(!ValidationMode::DefaultBlocking.freshness_blocks());
        assert!(ValidationMode::StrictBlocking.freshness_blocks());
    }

    #[test]
    fn mode_within_session_blocks_only_in_strict() {
        assert!(!ValidationMode::ObserveOnly.within_session_blocks());
        assert!(!ValidationMode::DefaultBlocking.within_session_blocks());
        assert!(ValidationMode::StrictBlocking.within_session_blocks());
    }
}
