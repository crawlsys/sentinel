//! Step-level verifier requirements (sentinel #71).
//!
//! Declarative pre-condition layer atop the proof chain. A
//! [`StepVerifierRequirement`] says: "step N+1 of skill X cannot
//! seal until step N's sealed proof carries a verified evidence
//! receipt from adapter Y." When the configured verifier is
//! `browserbase`, this realizes the "Browserbase as third-party
//! verifier" pattern (#71) — UI steps require a Browserbase
//! smoke-test receipt before downstream work can proceed.
//!
//! ## Why a separate layer
//!
//! The existing `step_gate` hook fires at PreToolUse on the tool
//! name, before the step runs. The verifier check fires at
//! `submit_step_complete`, after the step runs but before the
//! proof seals. Both layers are useful:
//!
//! - **step_gate**: "you didn't run the prior step at all" → blocks
//!   the *attempt* to fire the next step's tool.
//! - **step_verifier**: "you ran the prior step but its evidence
//!   doesn't carry the required receipt" → blocks the *seal* of
//!   the next step's proof.
//!
//! Defense in depth: a misconfigured tool dispatcher can bypass
//! step_gate (it's enforced at the Claude Code layer), but it
//! can't bypass step_verifier because the proof engine refuses to
//! seal without the receipt regardless of how the call arrived.
//!
//! ## Shape
//!
//! Requirements are matched on `(skill, phase_id, step_id)` —
//! the coordinates of the step being submitted. The required
//! receipt is identified by `adapter_name` (e.g. `"browserbase"`)
//! and optionally a `verified_only` flag forcing `verified == true`
//! (so a sealed-but-failed Browserbase receipt doesn't satisfy
//! the requirement — exactly the production case where you DON'T
//! want to ship after a smoke-test failure).
//!
//! ## Lookup convention
//!
//! Receipts live in `Evidence.custom.evidence_receipt` (the single
//! canonical key the BIBLE wireup uses, sentinel #68). Verifiers
//! look there; absence of the key fails the requirement with a
//! clear error.

use serde::{Deserialize, Serialize};

/// One step-level verifier requirement. Composed into a registry
/// at handler-construction time and consulted in submit_step_complete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepVerifierRequirement {
    /// Skill the requirement applies to (e.g. `"linear"`).
    pub skill: String,
    /// Phase id within the skill (e.g. `"qa-handoff"`).
    pub phase_id: String,
    /// Step id within the phase (e.g. `"3.5.5"` for the
    /// implementation-comment step — requires the smoke-test step
    /// 3.5.1 to have produced a Browserbase receipt).
    pub step_id: String,
    /// Required receipt's `adapter_name`. Matched exactly.
    pub adapter_name: String,
    /// When true, the receipt must also have `verified == true`.
    /// Defaults to true via the constructor — most callers want
    /// "the receipt exists AND it confirms the claim." Set to
    /// false when you only need provenance (e.g. for audit
    /// trails where the negative result is itself the data point).
    pub verified_only: bool,
}

impl StepVerifierRequirement {
    /// Standard constructor: requires a verified receipt from the
    /// named adapter at the named step coordinates.
    #[must_use]
    pub fn new(
        skill: impl Into<String>,
        phase_id: impl Into<String>,
        step_id: impl Into<String>,
        adapter_name: impl Into<String>,
    ) -> Self {
        Self {
            skill: skill.into(),
            phase_id: phase_id.into(),
            step_id: step_id.into(),
            adapter_name: adapter_name.into(),
            verified_only: true,
        }
    }

    /// Variant that accepts any receipt from the adapter, verified
    /// or not. Use for audit-only requirements where the act of
    /// producing the receipt matters more than the verdict.
    #[must_use]
    pub fn provenance_only(
        skill: impl Into<String>,
        phase_id: impl Into<String>,
        step_id: impl Into<String>,
        adapter_name: impl Into<String>,
    ) -> Self {
        Self {
            skill: skill.into(),
            phase_id: phase_id.into(),
            step_id: step_id.into(),
            adapter_name: adapter_name.into(),
            verified_only: false,
        }
    }

    /// True iff this requirement matches the given step coordinates.
    #[must_use]
    pub fn matches(&self, skill: &str, phase_id: &str, step_id: &str) -> bool {
        self.skill == skill && self.phase_id == phase_id && self.step_id == step_id
    }

    /// Evaluate the requirement against an `evidence.custom` payload.
    /// Returns `Ok(())` on satisfied, `Err(reason)` on failure.
    ///
    /// `evidence_custom` is the value of `Evidence.custom` for the
    /// step whose proof is about to seal — typically obtained via
    /// the BIBLE wireup that just folded the receipt in (sentinel #68).
    pub fn check(&self, evidence_custom: &serde_json::Value) -> Result<(), VerifierError> {
        let receipt = evidence_custom.get("evidence_receipt").ok_or_else(|| {
            VerifierError::MissingReceipt {
                required_adapter: self.adapter_name.clone(),
            }
        })?;

        let actual_adapter = receipt
            .get("adapter_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VerifierError::MalformedReceipt {
                detail: "receipt missing 'adapter_name' field".to_string(),
            })?;

        if actual_adapter != self.adapter_name {
            return Err(VerifierError::WrongAdapter {
                required: self.adapter_name.clone(),
                actual: actual_adapter.to_string(),
            });
        }

        if self.verified_only {
            let verified = receipt
                .get("verified")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| VerifierError::MalformedReceipt {
                    detail: "receipt missing 'verified' field (required when verified_only=true)"
                        .to_string(),
                })?;
            if !verified {
                return Err(VerifierError::ReceiptNotVerified {
                    adapter: self.adapter_name.clone(),
                });
            }
        }

        Ok(())
    }
}

/// Reasons a verifier requirement can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifierError {
    /// `Evidence.custom.evidence_receipt` is absent — no receipt
    /// was folded into the evidence.
    MissingReceipt { required_adapter: String },
    /// A receipt exists but its `adapter_name` doesn't match.
    /// Caller probably passed a claim to the wrong adapter.
    WrongAdapter { required: String, actual: String },
    /// A receipt exists, the adapter matches, but `verified=false`
    /// and the requirement is `verified_only=true`. The proof
    /// chain refuses to seal a downstream step on a failed receipt.
    ReceiptNotVerified { adapter: String },
    /// Receipt is structurally invalid — missing fields the
    /// verifier needs to evaluate the predicate.
    MalformedReceipt { detail: String },
}

impl std::fmt::Display for VerifierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingReceipt { required_adapter } => write!(
                f,
                "step verifier requires a receipt from adapter '{required_adapter}' \
                 but evidence.custom.evidence_receipt is absent"
            ),
            Self::WrongAdapter { required, actual } => write!(
                f,
                "step verifier requires a receipt from adapter '{required}' \
                 but the supplied receipt was from '{actual}'"
            ),
            Self::ReceiptNotVerified { adapter } => write!(
                f,
                "step verifier requires verified=true from adapter '{adapter}' \
                 but the supplied receipt has verified=false"
            ),
            Self::MalformedReceipt { detail } => write!(
                f,
                "step verifier received a malformed receipt: {detail}"
            ),
        }
    }
}

impl std::error::Error for VerifierError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_receipt(adapter: &str, verified: bool) -> serde_json::Value {
        serde_json::json!({
            "evidence_receipt": {
                "adapter_name": adapter,
                "verified": verified,
                "payload": {"x": 1}
            }
        })
    }

    #[test]
    fn matches_only_exact_coordinates() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        assert!(req.matches("linear", "qa-handoff", "3.5.5"));
        assert!(!req.matches("linear", "qa-handoff", "3.5.6"));
        assert!(!req.matches("linear", "claim", "3.5.5"));
        assert!(!req.matches("github", "qa-handoff", "3.5.5"));
    }

    #[test]
    fn check_passes_when_verified_receipt_from_correct_adapter() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        let custom = good_receipt("browserbase", true);
        assert_eq!(req.check(&custom), Ok(()));
    }

    #[test]
    fn check_fails_when_receipt_missing() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        let custom = serde_json::json!({});
        let err = req.check(&custom).unwrap_err();
        assert!(
            matches!(&err, VerifierError::MissingReceipt { required_adapter } if required_adapter == "browserbase"),
            "got: {err:?}"
        );
    }

    #[test]
    fn check_fails_when_adapter_name_does_not_match() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        let custom = good_receipt("filesystem", true);
        let err = req.check(&custom).unwrap_err();
        assert!(matches!(err, VerifierError::WrongAdapter { .. }), "got: {err:?}");
    }

    #[test]
    fn check_fails_when_verified_only_and_receipt_not_verified() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        let custom = good_receipt("browserbase", false);
        let err = req.check(&custom).unwrap_err();
        assert!(
            matches!(&err, VerifierError::ReceiptNotVerified { adapter } if adapter == "browserbase"),
            "got: {err:?}"
        );
    }

    #[test]
    fn provenance_only_accepts_unverified_receipt() {
        let req = StepVerifierRequirement::provenance_only(
            "linear",
            "qa-handoff",
            "3.5.5",
            "browserbase",
        );
        let custom = good_receipt("browserbase", false);
        // Unverified receipt is fine in provenance-only mode — the
        // act of producing the receipt is what's being witnessed.
        assert_eq!(req.check(&custom), Ok(()));
    }

    #[test]
    fn check_fails_on_malformed_receipt_missing_adapter_name() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        let custom = serde_json::json!({
            "evidence_receipt": {"verified": true}
        });
        let err = req.check(&custom).unwrap_err();
        assert!(matches!(err, VerifierError::MalformedReceipt { .. }), "got: {err:?}");
    }

    #[test]
    fn check_fails_on_malformed_receipt_missing_verified_field() {
        let req = StepVerifierRequirement::new("linear", "qa-handoff", "3.5.5", "browserbase");
        let custom = serde_json::json!({
            "evidence_receipt": {"adapter_name": "browserbase"}
        });
        let err = req.check(&custom).unwrap_err();
        assert!(matches!(err, VerifierError::MalformedReceipt { .. }), "got: {err:?}");
    }

    #[test]
    fn display_strings_are_human_readable() {
        let cases: Vec<(VerifierError, &str)> = vec![
            (
                VerifierError::MissingReceipt { required_adapter: "browserbase".into() },
                "browserbase",
            ),
            (
                VerifierError::WrongAdapter {
                    required: "browserbase".into(),
                    actual: "filesystem".into(),
                },
                "filesystem",
            ),
            (
                VerifierError::ReceiptNotVerified { adapter: "browserbase".into() },
                "verified=true",
            ),
            (
                VerifierError::MalformedReceipt { detail: "x".into() },
                "malformed",
            ),
        ];
        for (err, substring) in cases {
            let s = err.to_string();
            assert!(s.contains(substring), "Display for {err:?} missing '{substring}': {s}");
        }
    }
}
