//! `WitnessVerifierPort` -- pluggable verification of
//! `VoiceprintWitness` payloads arriving on inbound
//! `CatastrophicAck` messages.
//!
//! # Why a separate port
//!
//! sentinel-legatus is a low-level crate that owns the WS recv
//! path; it can't directly depend on sentinel-application
//! (circular). The actual cryptographic verifier
//! (PraefectusClient-backed: Ed25519 signature + the 6-step
//! Praefectus check) lives in sentinel-application. The daemon
//! (sentinel-cli) owns the wiring: it constructs an adapter
//! implementing this port that delegates to the
//! sentinel-application PraefectusClient, and hands the Arc to
//! the LegatusRuntime via
//! [`crate::handle::LegatusRuntime::with_witness_verifier`].
//!
//! # Default behavior when no verifier is wired
//!
//! `LegatusRuntime::approval_cache` is `None`-by-default on
//! standalone CLI paths; the daemon path always sets it. The
//! verifier is similarly optional:
//!
//! - **No verifier installed** (`approval_cache.verifier ==
//!   None`): handle_inbound records the approval unconditionally.
//!   Preserves v0.1 daemon-local trust semantics (the cache lives
//!   in-process behind bearer-auth on localhost; the threat model
//!   is "anyone with shell access" which already owns the
//!   machine).
//! - **Verifier installed**: handle_inbound calls
//!   `verifier.verify(&witness, escalation_ref)` BEFORE recording.
//!   On `Err`, the approval is dropped + logged.
//!   On `Ok`, the approval is recorded.
//!
//! Production sentinel deployments that have a Praefectus
//! reachable SHOULD wire a real verifier. The hook flow then
//! becomes end-to-end cryptographically attested.
//!
//! # Bundled adapters
//!
//! - [`AlwaysAccept`] -- accepts every witness; matches the
//!   no-verifier behavior but makes the choice explicit at
//!   wiring time. Use for dev / tests / migration windows.
//! - [`AlwaysReject`] -- rejects every witness with a reason
//!   string. Use as a fail-closed circuit-breaker; the cache
//!   never receives writes, so no retry is ever auto-allowed.

#![allow(clippy::missing_errors_doc)]

use std::fmt;

use consul_domain::identity::republic::VoiceprintWitness;
use consul_protocol::messages::EscalationKey;

/// Error returned by [`WitnessVerifierPort::verify`].
#[derive(Debug)]
pub struct WitnessVerificationError {
    /// Operator-facing diagnostic. Logged at warn level.
    pub reason: String,
}

impl fmt::Display for WitnessVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "witness verification failed: {}", self.reason)
    }
}

impl std::error::Error for WitnessVerificationError {}

/// Pluggable verifier for inbound `CatastrophicAck` witnesses.
///
/// Async because production adapters need to round-trip to a
/// remote Praefectus. The default sentinel-legatus handle_inbound
/// invocation pattern spawns the verifier as a `tokio::spawn`
/// task so the synchronous WS recv loop isn't blocked while a
/// verification is in flight; the record-into-cache step happens
/// inside that task after the verification resolves.
#[async_trait::async_trait]
pub trait WitnessVerifierPort: Send + Sync {
    /// Verify `witness` against the operator identity it claims
    /// and the `escalation_key` it pretends to authorize.
    ///
    /// On `Ok(())` the caller records the approval. On `Err`,
    /// the approval is dropped + logged. Implementations SHOULD
    /// gate on a short per-call timeout so a hung Praefectus
    /// can't pile up unbounded pending verifications.
    async fn verify(
        &self,
        witness: &VoiceprintWitness,
        escalation_key: &EscalationKey,
    ) -> Result<(), WitnessVerificationError>;
}

/// Test / dev adapter that accepts every witness. Equivalent to
/// no verifier wired at all, but lets the wiring code make the
/// choice explicit (and lets tests inject deterministic accept).
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysAccept;

#[async_trait::async_trait]
impl WitnessVerifierPort for AlwaysAccept {
    async fn verify(
        &self,
        _witness: &VoiceprintWitness,
        _escalation_key: &EscalationKey,
    ) -> Result<(), WitnessVerificationError> {
        Ok(())
    }
}

/// Fail-closed adapter: rejects every witness with a fixed
/// reason. Use as a circuit-breaker when a Praefectus is
/// configured-but-unreachable, or in security-sensitive
/// deployments that haven't yet wired a real verifier and want
/// to ensure no CatastrophicAcks auto-allow retries.
#[derive(Debug, Clone)]
pub struct AlwaysReject {
    reason: String,
}

impl AlwaysReject {
    /// Construct with the rejection reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Default for AlwaysReject {
    fn default() -> Self {
        Self::new("verifier configured to reject all witnesses (fail-closed)")
    }
}

#[async_trait::async_trait]
impl WitnessVerifierPort for AlwaysReject {
    async fn verify(
        &self,
        _witness: &VoiceprintWitness,
        _escalation_key: &EscalationKey,
    ) -> Result<(), WitnessVerificationError> {
        Err(WitnessVerificationError {
            reason: self.reason.clone(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use chrono::Utc;
    use consul_domain::identity::republic::{ChallengeNonce, OperatorId};
    use consul_domain::identity::SessionId;
    use consul_protocol::messages::EscalationKey;
    use uuid::Uuid;

    use super::*;

    fn fixture_witness() -> VoiceprintWitness {
        VoiceprintWitness {
            operator: OperatorId::from_uuid(Uuid::from_bytes([0xAA; 16])),
            utterance_audio_hash: [0x11; 32],
            utterance_transcript: "approve deploy, code 1234".into(),
            challenge_nonce: ChallengeNonce::from_bytes([0x77; 16]),
            signature: [0x22; 64],
            signed_at: Utc::now(),
        }
    }

    fn fixture_key() -> EscalationKey {
        EscalationKey::SessionBlocked {
            session_id: SessionId::from_uuid(Uuid::from_bytes([0xBB; 16])),
            detected_at_ms: 1_700_000_000_000,
        }
    }

    #[tokio::test]
    async fn always_accept_returns_ok() {
        let v = AlwaysAccept;
        assert!(v.verify(&fixture_witness(), &fixture_key()).await.is_ok());
    }

    #[tokio::test]
    async fn always_reject_default_returns_err_with_default_reason() {
        let v = AlwaysReject::default();
        let r = v.verify(&fixture_witness(), &fixture_key()).await;
        let err = r.unwrap_err();
        assert!(err.reason.contains("fail-closed"), "got: {err}");
    }

    #[tokio::test]
    async fn always_reject_carries_caller_supplied_reason() {
        let v = AlwaysReject::new("praefectus unreachable");
        let err = v
            .verify(&fixture_witness(), &fixture_key())
            .await
            .unwrap_err();
        assert!(err.reason.contains("praefectus unreachable"));
    }

    #[test]
    fn verifier_error_displays_with_prefix() {
        let err = WitnessVerificationError {
            reason: "bad signature".into(),
        };
        assert_eq!(
            format!("{err}"),
            "witness verification failed: bad signature"
        );
    }
}
