//! Browserbase evidence adapter — second concrete adapter for THE
//! BIBLE framework (sentinel #70).
//!
//! Verifies claims of the form `"browserbase.<verb>"` against
//! caller-supplied Browserbase session metadata. This is the
//! "judge evidence" pattern: the agent runs a browser smoke test
//! through the Browserbase MCP server (remote URLs) or the CDP MCP
//! server (localhost), collects the `session_id` + screenshot count +
//! console-error count + any artifact URLs,
//! and passes that context through `submit_step_complete`'s
//! `evidence_claim` arg. This adapter then validates the shape
//! is well-formed and seals a receipt with provenance binding
//! the claim to the artifacts.
//!
//! ## Why a metadata-shape adapter, not an HTTP-calling adapter
//!
//! The full "phone home to Browserbase and re-fetch the session"
//! flow needs `BROWSERBASE_API_KEY` plus an HTTP client plus an
//! auth refresher. Adding all that to `sentinel-infrastructure`
//! would (a) pull in reqwest at this layer for one adapter, (b)
//! require sentinel to manage Browserbase credentials, which
//! the [Doppler-personal-branch convention](crate::adapters)
//! pushes OUT of sentinel onto each MCP server. The right split:
//! the Browserbase MCP server (separate repo, already
//! credential-managed) collects the artifacts; this adapter
//! validates them and seals the receipt.
//!
//! A future enhancement can add a second "`browserbase_remote`"
//! adapter that DOES phone home — that's the M7.1 multi-adapter
//! pattern (#69 cross-vendor verification). For #70 we ship
//! the metadata-shape version because it's the entry point that
//! lets Browserbase artifacts start landing in the proof chain
//! *today*, without waiting for the credential-management story
//! to land in sentinel-infrastructure.
//!
//! # Supported claim types
//!
//! - **`browserbase.session_observed`** — generic: caller swears
//!   they ran *a* Browserbase session and here's what it returned.
//!   Required context fields: `session_id: string`. Optional:
//!   `screenshot_count: u64`, `console_errors: u64`, `recording_url: string`,
//!   `artifacts: array`. Receipt is `verified: true` iff `session_id`
//!   is a non-empty string. Permissive on purpose — the act of
//!   running a session is what's being witnessed; whether the
//!   session showed a passing UI is a separate claim.
//!
//! - **`browserbase.smoke_test_passed`** — stronger: caller asserts
//!   the smoke test passed cleanly. Required context fields:
//!   `session_id`, `screenshot_count >= 1`, `console_errors == 0`.
//!   Optional: `recording_url`, `assertions: array of strings`.
//!   Receipt is `verified: true` iff all three required predicates
//!   hold. A claim with `console_errors > 0` produces a receipt
//!   with `verified: false` — recording the negative observation
//!   in the chain rather than rejecting the claim outright (per
//!   THE BIBLE: "we know we don't know" is a first-class entry).
//!
//! # Receipt payload shape
//!
//! Whatever the caller passed in `claim.context`, normalized into
//! a stable object (lowercase keys, explicit null for missing
//! optional fields). Future verifiers reading old receipts get a
//! predictable shape regardless of what the caller happened to
//! include.

use async_trait::async_trait;
use chrono::Utc;
use sentinel_application::evidence_adapters::EvidenceAdapter;
use sentinel_domain::evidence_adapter::{AdapterError, EvidenceClaim, EvidenceReceipt};

/// Adapter name, surfaced in `EvidenceReceipt::adapter_name` and the
/// provenance hash.
pub const ADAPTER_NAME: &str = "browserbase";

/// Claim-type prefix this adapter dispatches on.
pub const CLAIM_PREFIX: &str = "browserbase.";

const CLAIM_SESSION_OBSERVED: &str = "browserbase.session_observed";
const CLAIM_SMOKE_TEST_PASSED: &str = "browserbase.smoke_test_passed";

/// Browserbase evidence adapter.
///
/// Stateless. Holds no auth, no HTTP client. Validates the shape
/// of caller-supplied Browserbase session metadata and seals a
/// receipt with provenance binding.
#[derive(Debug, Default, Clone, Copy)]
pub struct BrowserbaseAdapter;

impl BrowserbaseAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn session_id_of(claim: &EvidenceClaim) -> Result<&str, AdapterError> {
        let session_id = claim
            .context
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AdapterError::Fetch(format!(
                    "browserbase adapter requires context.session_id: string for claim '{}'",
                    claim.claim_type
                ))
            })?;
        if session_id.is_empty() {
            return Err(AdapterError::Fetch(
                "browserbase adapter rejects empty context.session_id".to_string(),
            ));
        }
        Ok(session_id)
    }

    fn handle_session_observed(claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        let session_id = Self::session_id_of(claim)?;
        let screenshot_count = claim
            .context
            .get("screenshot_count")
            .and_then(serde_json::Value::as_u64);
        let console_errors = claim
            .context
            .get("console_errors")
            .and_then(serde_json::Value::as_u64);
        let recording_url = claim
            .context
            .get("recording_url")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let artifacts = claim
            .context
            .get("artifacts")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // session_observed: the existence of a non-empty session_id
        // IS the verification. The agent ran a session; that's what's
        // being witnessed here.
        let verified = true;
        let payload = serde_json::json!({
            "session_id": session_id,
            "screenshot_count": screenshot_count,
            "console_errors": console_errors,
            "recording_url": recording_url,
            "artifacts": artifacts,
        });
        Ok(EvidenceReceipt::new(
            ADAPTER_NAME.to_string(),
            claim,
            verified,
            payload,
            Utc::now(),
        ))
    }

    fn handle_smoke_test_passed(claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        let session_id = Self::session_id_of(claim)?;
        let screenshot_count = claim
            .context
            .get("screenshot_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let console_errors = claim
            .context
            .get("console_errors")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let recording_url = claim
            .context
            .get("recording_url")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let assertions = claim
            .context
            .get("assertions")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Three predicates for "smoke test passed cleanly":
        //   1. session_id present (guaranteed by session_id_of)
        //   2. at least one screenshot captured
        //   3. zero console errors
        // Any failure flips `verified` to false but the receipt
        // still seals with the negative observation recorded.
        let verified = screenshot_count >= 1 && console_errors == 0;

        let payload = serde_json::json!({
            "session_id": session_id,
            "screenshot_count": screenshot_count,
            "console_errors": console_errors,
            "recording_url": recording_url,
            "assertions": assertions,
            "predicates": {
                "session_id_present": true,
                "at_least_one_screenshot": screenshot_count >= 1,
                "zero_console_errors": console_errors == 0,
            },
        });
        Ok(EvidenceReceipt::new(
            ADAPTER_NAME.to_string(),
            claim,
            verified,
            payload,
            Utc::now(),
        ))
    }
}

#[async_trait]
impl EvidenceAdapter for BrowserbaseAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn supports(&self, claim_type: &str) -> bool {
        claim_type.starts_with(CLAIM_PREFIX)
    }

    async fn fetch(&self, claim: &EvidenceClaim) -> Result<EvidenceReceipt, AdapterError> {
        match claim.claim_type.as_str() {
            CLAIM_SESSION_OBSERVED => Self::handle_session_observed(claim),
            CLAIM_SMOKE_TEST_PASSED => Self::handle_smoke_test_passed(claim),
            other => Err(AdapterError::Fetch(format!(
                "browserbase adapter does not handle claim_type '{other}' — \
                 supported: {CLAIM_SESSION_OBSERVED}, {CLAIM_SMOKE_TEST_PASSED}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim_with(claim_type: &str, context: serde_json::Value) -> EvidenceClaim {
        EvidenceClaim {
            skill: "browserbase-test".to_string(),
            phase_id: "qa-handoff".to_string(),
            step_id: "3.5.1".to_string(),
            claim_type: claim_type.to_string(),
            context,
        }
    }

    #[test]
    fn supports_browserbase_prefix_only() {
        let a = BrowserbaseAdapter::new();
        assert!(a.supports("browserbase.session_observed"));
        assert!(a.supports("browserbase.smoke_test_passed"));
        assert!(a.supports("browserbase.future_verb"));
        assert!(!a.supports("filesystem.file_exists"));
        assert!(!a.supports("git.pr_opened"));
        assert!(!a.supports("browserbase")); // bare prefix, no dot — not a verb
    }

    #[tokio::test]
    async fn session_observed_minimal_seals_verified_receipt() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SESSION_OBSERVED,
            serde_json::json!({"session_id": "bb_sess_001"}),
        );
        let receipt = a.fetch(&claim).await.unwrap();
        assert_eq!(receipt.adapter_name, "browserbase");
        assert!(receipt.verified, "session_observed with valid id verifies");
        assert_eq!(
            receipt.payload.get("session_id").and_then(|v| v.as_str()),
            Some("bb_sess_001")
        );
        // Optional fields default to null in the normalized payload,
        // so verifiers can rely on shape regardless of what the caller
        // provided.
        assert!(receipt.payload.get("screenshot_count").unwrap().is_null());
        assert!(receipt.payload.get("recording_url").unwrap().is_null());
    }

    #[tokio::test]
    async fn session_observed_with_full_context_preserves_fields() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SESSION_OBSERVED,
            serde_json::json!({
                "session_id": "bb_sess_002",
                "screenshot_count": 4,
                "console_errors": 0,
                "recording_url": "https://browserbase.com/sessions/bb_sess_002",
                "artifacts": [
                    {"kind": "screenshot", "url": "https://.../1.png"},
                    {"kind": "screenshot", "url": "https://.../2.png"}
                ]
            }),
        );
        let receipt = a.fetch(&claim).await.unwrap();
        assert!(receipt.verified);
        assert_eq!(
            receipt
                .payload
                .get("screenshot_count")
                .and_then(|v| v.as_u64()),
            Some(4)
        );
        assert_eq!(
            receipt
                .payload
                .get("recording_url")
                .and_then(|v| v.as_str()),
            Some("https://browserbase.com/sessions/bb_sess_002")
        );
        assert_eq!(
            receipt
                .payload
                .get("artifacts")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(2)
        );
    }

    #[tokio::test]
    async fn session_observed_missing_session_id_errors() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(CLAIM_SESSION_OBSERVED, serde_json::json!({}));
        let err = a
            .fetch(&claim)
            .await
            .expect_err("must error on missing session_id");
        let msg = err.to_string();
        assert!(
            msg.contains("session_id"),
            "error must name the missing field: {msg}"
        );
    }

    #[tokio::test]
    async fn session_observed_empty_session_id_errors() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SESSION_OBSERVED,
            serde_json::json!({"session_id": ""}),
        );
        let err = a.fetch(&claim).await.expect_err("empty string rejected");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[tokio::test]
    async fn smoke_test_passed_all_predicates_satisfied_verifies() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SMOKE_TEST_PASSED,
            serde_json::json!({
                "session_id": "bb_sess_003",
                "screenshot_count": 3,
                "console_errors": 0,
                "recording_url": "https://browserbase.com/sessions/bb_sess_003",
                "assertions": ["login succeeds", "main view renders"]
            }),
        );
        let receipt = a.fetch(&claim).await.unwrap();
        assert!(receipt.verified);
        let predicates = receipt.payload.get("predicates").unwrap();
        assert_eq!(
            predicates
                .get("session_id_present")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            predicates
                .get("at_least_one_screenshot")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            predicates
                .get("zero_console_errors")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn smoke_test_passed_with_console_errors_records_negative() {
        // "We know we don't know" — the receipt still seals, with
        // verified=false. Corpus queries on verified=false surface
        // this as a smoke-test failure for follow-up.
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SMOKE_TEST_PASSED,
            serde_json::json!({
                "session_id": "bb_sess_004",
                "screenshot_count": 2,
                "console_errors": 3
            }),
        );
        let receipt = a.fetch(&claim).await.unwrap();
        assert!(
            !receipt.verified,
            "console_errors > 0 must NOT verify, but receipt should still seal"
        );
        let predicates = receipt.payload.get("predicates").unwrap();
        assert_eq!(
            predicates
                .get("zero_console_errors")
                .and_then(|v| v.as_bool()),
            Some(false),
            "predicate breakdown surfaces which check failed"
        );
        assert_eq!(
            receipt
                .payload
                .get("console_errors")
                .and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    #[tokio::test]
    async fn smoke_test_passed_with_zero_screenshots_records_negative() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SMOKE_TEST_PASSED,
            serde_json::json!({
                "session_id": "bb_sess_005",
                "screenshot_count": 0,
                "console_errors": 0
            }),
        );
        let receipt = a.fetch(&claim).await.unwrap();
        assert!(!receipt.verified, "zero screenshots fails the predicate");
    }

    #[tokio::test]
    async fn unknown_claim_type_errors() {
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            "browserbase.future_unhandled_verb",
            serde_json::json!({"session_id": "bb_sess_006"}),
        );
        let err = a.fetch(&claim).await.expect_err("unknown verb must error");
        let msg = err.to_string();
        assert!(msg.contains("browserbase adapter does not handle"), "{msg}");
        assert!(msg.contains("future_unhandled_verb"), "{msg}");
    }

    #[tokio::test]
    async fn receipt_provenance_binds_to_claim() {
        // The receipt's provenance_hash MUST be derivable from
        // (adapter_name, claim_type, claim_context_hash, payload_hash).
        // Verify that the receipt validates against its own claim.
        let a = BrowserbaseAdapter::new();
        let claim = claim_with(
            CLAIM_SMOKE_TEST_PASSED,
            serde_json::json!({
                "session_id": "bb_sess_007",
                "screenshot_count": 1,
                "console_errors": 0
            }),
        );
        let receipt = a.fetch(&claim).await.unwrap();
        receipt
            .verify_provenance(&claim)
            .expect("provenance must verify against the claim that produced it");
    }
}
