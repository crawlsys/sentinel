//! BA1 — Audit-extract hook (`PostToolUse`).
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §2.1. Lifts every
//! successful documentation-connector call (`mcp__*` tools that emit
//! a structured provenance audit event per BA6 §3.4) into sentinel's
//! connector-audit chain via the [`ProvenanceWritePort`]. The future
//! `provenance_validate` hook (Phase 3b) reads from the same chain
//! to validate BA-orchestrator citations.
//!
//! ## Observational, never blocking
//!
//! `audit_extract` runs on `PostToolUse` and ALWAYS returns
//! [`HookOutput::allow()`]. The tool call has already completed when
//! this hook fires; blocking would be incoherent. Write failures
//! emit a `tracing::warn` and drop the record — the downstream
//! `provenance_validate` hook treats missing-record as a Block-class
//! finding regardless of cause (connector wasn't called OR audit
//! write failed), so dropping silently here is the right behavior.
//!
//! ## Connector audit-event shape
//!
//! BA6 specifies that documentation connectors emit a structured
//! `provenance_audit` field in their `PostToolUse` `extra` payload:
//!
//! ```json
//! {
//!   "provenance_audit": {
//!     "artifact_id": "FIR-123",
//!     "content_hash": "abcd1234",
//!     "provenance_class": "SystemOfRecord"
//!   }
//! }
//! ```
//!
//! The hook fills in `connector_name` from the tool name (e.g.,
//! `mcp__linear__get_issue`), `session_id` from the hook input, and
//! `retrieved_at` from `chrono::Utc::now()`. Connectors that DON'T
//! emit the audit field are silently skipped — they aren't
//! registered as documentation connectors per BA6's classification.

use chrono::Utc;
use sentinel_domain::ba::{ProvenanceClass, RetrievalRecord};
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::ProvenanceWritePort;

/// Tool-name prefix that identifies MCP server tools.
const MCP_PREFIX: &str = "mcp__";

/// Process a `PostToolUse` event for connector audit lift.
///
/// `provenance_writer` is the persistence port. Tests inject a
/// recording stub; production passes the JSONL adapter (Phase 4).
#[must_use]
pub fn process(input: &HookInput, provenance_writer: &dyn ProvenanceWritePort) -> HookOutput {
    let Some(tool_name) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };
    if !tool_name.starts_with(MCP_PREFIX) {
        return HookOutput::allow();
    }
    let Some(audit_value) = input.extra.get("provenance_audit") else {
        // Connector didn't emit a structured audit event — either
        // not a documentation connector OR an older connector version
        // that doesn't yet emit. Silently skip; no record to lift.
        return HookOutput::allow();
    };
    let parsed = match parse_audit_event(audit_value) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(
                tool = %tool_name,
                error = %err,
                "audit_extract: malformed provenance_audit; skipping"
            );
            return HookOutput::allow();
        }
    };
    let Some(session_id) = input.session_id.as_deref().filter(|s| !s.is_empty()) else {
        tracing::warn!(
            tool = %tool_name,
            "audit_extract: missing session_id; cannot persist retrieval record"
        );
        return HookOutput::allow();
    };
    let record = RetrievalRecord {
        artifact_id: parsed.artifact_id,
        connector_name: tool_name.to_string(),
        content_hash: parsed.content_hash,
        provenance_class: parsed.provenance_class,
        session_id: session_id.to_string(),
        retrieved_at: Utc::now(),
    };
    if let Err(err) = provenance_writer.record(record) {
        tracing::warn!(
            tool = %tool_name,
            error = %err,
            "audit_extract: failed to persist retrieval record"
        );
    }
    HookOutput::allow()
}

/// Parsed shape of the `provenance_audit` JSON field — connector
/// supplies `artifact_id` + `content_hash` + `provenance_class`;
/// the hook adds the rest.
#[derive(Debug, PartialEq)]
struct ParsedAuditEvent {
    artifact_id: String,
    content_hash: String,
    provenance_class: ProvenanceClass,
}

fn parse_audit_event(value: &serde_json::Value) -> Result<ParsedAuditEvent, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "provenance_audit must be an object".to_string())?;
    let artifact_id = obj
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing or empty artifact_id".to_string())?
        .to_string();
    let content_hash = obj
        .get("content_hash")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing or empty content_hash".to_string())?
        .to_string();
    let class_str = obj
        .get("provenance_class")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing provenance_class".to_string())?;
    let provenance_class = match class_str {
        "SystemOfRecord" => ProvenanceClass::SystemOfRecord,
        "ExpertSynthesis" => ProvenanceClass::ExpertSynthesis,
        "Inference" => ProvenanceClass::Inference,
        "Unverified" => ProvenanceClass::Unverified,
        other => return Err(format!("unknown provenance_class {other:?}")),
    };
    Ok(ParsedAuditEvent {
        artifact_id,
        content_hash,
        provenance_class,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::ports::ProvenanceError;
    use std::sync::Mutex;

    /// Recording-stub `ProvenanceWritePort` for tests. Records every
    /// `record()` call into an internal `Vec` plus an optional canned
    /// error to return on the next call.
    struct StubWriter {
        records: Mutex<Vec<RetrievalRecord>>,
        next_error: Mutex<Option<ProvenanceError>>,
    }

    impl StubWriter {
        fn new() -> Self {
            Self {
                records: Mutex::new(Vec::new()),
                next_error: Mutex::new(None),
            }
        }

        fn with_next_error(self, err: ProvenanceError) -> Self {
            *self.next_error.lock().unwrap() = Some(err);
            self
        }

        fn recorded(&self) -> Vec<RetrievalRecord> {
            self.records.lock().unwrap().clone()
        }
    }

    impl ProvenanceWritePort for StubWriter {
        fn record(&self, record: RetrievalRecord) -> Result<(), ProvenanceError> {
            if let Some(err) = self.next_error.lock().unwrap().take() {
                return Err(err);
            }
            self.records.lock().unwrap().push(record);
            Ok(())
        }
    }

    fn mcp_input(tool: &str, session_id: &str, audit: Option<serde_json::Value>) -> HookInput {
        let mut extra = serde_json::Map::new();
        if let Some(v) = audit {
            extra.insert("provenance_audit".to_string(), v);
        }
        HookInput {
            tool_name: Some(tool.to_string()),
            session_id: Some(session_id.to_string()),
            extra,
            ..Default::default()
        }
    }

    fn valid_audit() -> serde_json::Value {
        serde_json::json!({
            "artifact_id": "FIR-123",
            "content_hash": "abcd1234",
            "provenance_class": "SystemOfRecord"
        })
    }

    // ---- Allow-only contract ----

    #[test]
    fn always_returns_allow_even_with_no_tool_name() {
        let writer = StubWriter::new();
        let input = HookInput::default();
        let output = process(&input, &writer);
        assert_eq!(output.blocked, None);
    }

    #[test]
    fn always_returns_allow_for_non_mcp_tool() {
        let writer = StubWriter::new();
        let mut input = HookInput::default();
        input.tool_name = Some("Bash".to_string());
        let output = process(&input, &writer);
        assert_eq!(output.blocked, None);
        assert!(
            writer.recorded().is_empty(),
            "non-MCP tools must not produce records"
        );
    }

    // ---- Skip paths (no record written) ----

    #[test]
    fn non_mcp_tool_writes_nothing() {
        let writer = StubWriter::new();
        let input = mcp_input("Edit", "session-1", Some(valid_audit()));
        let _ = process(&input, &writer);
        assert!(
            writer.recorded().is_empty(),
            "Edit is not an mcp__ tool — no record"
        );
    }

    #[test]
    fn mcp_tool_without_audit_event_writes_nothing() {
        let writer = StubWriter::new();
        let input = mcp_input("mcp__linear__get_issue", "session-1", None);
        let _ = process(&input, &writer);
        assert!(
            writer.recorded().is_empty(),
            "MCP tool without provenance_audit field — silently skipped (not a documentation connector)"
        );
    }

    #[test]
    fn malformed_audit_event_writes_nothing() {
        let writer = StubWriter::new();
        let input = mcp_input(
            "mcp__linear__get_issue",
            "session-1",
            Some(serde_json::json!("not an object")),
        );
        let output = process(&input, &writer);
        assert_eq!(output.blocked, None);
        assert!(writer.recorded().is_empty());
    }

    #[test]
    fn missing_artifact_id_writes_nothing() {
        let writer = StubWriter::new();
        let bad = serde_json::json!({
            "content_hash": "h",
            "provenance_class": "SystemOfRecord"
        });
        let input = mcp_input("mcp__linear__get_issue", "session-1", Some(bad));
        let _ = process(&input, &writer);
        assert!(writer.recorded().is_empty());
    }

    #[test]
    fn unknown_provenance_class_writes_nothing() {
        let writer = StubWriter::new();
        let bad = serde_json::json!({
            "artifact_id": "x",
            "content_hash": "h",
            "provenance_class": "TotallyMadeUp"
        });
        let input = mcp_input("mcp__linear__get_issue", "session-1", Some(bad));
        let _ = process(&input, &writer);
        assert!(writer.recorded().is_empty());
    }

    #[test]
    fn missing_session_id_writes_nothing() {
        let writer = StubWriter::new();
        let mut input = mcp_input("mcp__linear__get_issue", "", Some(valid_audit()));
        input.session_id = None;
        let _ = process(&input, &writer);
        assert!(writer.recorded().is_empty());
    }

    #[test]
    fn empty_session_id_writes_nothing() {
        let writer = StubWriter::new();
        let input = mcp_input("mcp__linear__get_issue", "", Some(valid_audit()));
        let _ = process(&input, &writer);
        assert!(writer.recorded().is_empty());
    }

    // ---- Happy path ----

    #[test]
    fn valid_audit_event_persists_record() {
        let writer = StubWriter::new();
        let input = mcp_input("mcp__linear__get_issue", "session-42", Some(valid_audit()));
        let output = process(&input, &writer);
        assert_eq!(output.blocked, None);
        let recorded = writer.recorded();
        assert_eq!(recorded.len(), 1);
        let r = &recorded[0];
        assert_eq!(r.artifact_id, "FIR-123");
        assert_eq!(r.connector_name, "mcp__linear__get_issue");
        assert_eq!(r.content_hash, "abcd1234");
        assert_eq!(r.provenance_class, ProvenanceClass::SystemOfRecord);
        assert_eq!(r.session_id, "session-42");
    }

    #[test]
    fn fills_in_connector_name_from_tool_name() {
        let writer = StubWriter::new();
        let input = mcp_input(
            "mcp__confluence__get_page",
            "session-1",
            Some(valid_audit()),
        );
        let _ = process(&input, &writer);
        let recorded = writer.recorded();
        assert_eq!(recorded[0].connector_name, "mcp__confluence__get_page");
    }

    #[test]
    fn all_four_provenance_classes_parse() {
        let writer = StubWriter::new();
        for (class_str, expected) in [
            ("SystemOfRecord", ProvenanceClass::SystemOfRecord),
            ("ExpertSynthesis", ProvenanceClass::ExpertSynthesis),
            ("Inference", ProvenanceClass::Inference),
            ("Unverified", ProvenanceClass::Unverified),
        ] {
            let audit = serde_json::json!({
                "artifact_id": format!("art-{class_str}"),
                "content_hash": "h",
                "provenance_class": class_str
            });
            let input = mcp_input("mcp__test__get", "session-1", Some(audit));
            let _ = process(&input, &writer);
            let recorded = writer.recorded();
            let last = recorded.last().unwrap();
            assert_eq!(last.provenance_class, expected);
        }
    }

    // ---- Best-effort persistence ----

    #[test]
    fn write_failure_does_not_propagate_to_caller() {
        let writer =
            StubWriter::new().with_next_error(ProvenanceError::StoreUnavailable("disk".into()));
        let input = mcp_input("mcp__linear__get_issue", "session-1", Some(valid_audit()));
        let output = process(&input, &writer);
        assert_eq!(
            output.blocked, None,
            "write failures must NOT propagate as Block — observational hook"
        );
        assert!(
            writer.recorded().is_empty(),
            "the canned error consumed the call so no record landed"
        );
    }
}
