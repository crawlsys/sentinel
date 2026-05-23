//! Production `WitnessVerifierPort` adapter that delegates to the
//! `PraefectusClient`.
//!
//! sentinel-legatus owns the WitnessVerifierPort trait but cannot
//! depend on sentinel-application (sentinel-application depends on
//! sentinel-legatus). This crate (sentinel-application) is the
//! right home for the bridge adapter: it has both `PraefectusClient`
//! (the IPC surface to the per-machine Praefectus) and the
//! WitnessVerifierPort trait (re-exported from sentinel-legatus).
//!
//! The daemon (sentinel-cli) constructs this adapter at startup
//! when a Praefectus client is configured and installs it on the
//! `LegatusRuntime` via `with_witness_verifier`. Sentinel-legatus's
//! inbound `CatastrophicAck` handler then verifies every witness
//! cryptographically before recording an approval.

#![allow(clippy::missing_errors_doc)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use consul_domain::identity::republic::VoiceprintWitness;
use sentinel_legatus::{EscalationKey, WitnessVerificationError, WitnessVerifierPort};

use crate::praefectus_client::{EscalationRef, PraefectusClient};

/// Default per-verification timeout. The Praefectus is per-machine,
/// so RTT is near-zero in production; this cap is for hung HTTP /
/// IPC paths.
pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// Bridge: `WitnessVerifierPort` -> `PraefectusClient`.
///
/// Wraps any [`PraefectusClient`] implementation. The verifier:
///
/// 1. Derives an [`EscalationRef`] from the wire-protocol
///    [`EscalationKey`] (deterministic — same key always yields
///    the same ref).
/// 2. Calls `client.verify_voiceprint_witness(witness, operator,
///    escalation_ref)` under a `tokio::time::timeout` bound.
/// 3. Maps the result back to the `WitnessVerifierPort` shape.
///
/// Timeout / unreachable / verification-failed all map to
/// `WitnessVerificationError` (the approval is dropped).
pub struct PraefectusClientWitnessVerifier {
    client: Arc<dyn PraefectusClient>,
    timeout: Duration,
}

impl PraefectusClientWitnessVerifier {
    /// Construct with the default timeout.
    #[must_use]
    pub fn new(client: Arc<dyn PraefectusClient>) -> Self {
        Self::with_timeout(client, DEFAULT_VERIFY_TIMEOUT)
    }

    /// Construct with a custom timeout.
    #[must_use]
    pub fn with_timeout(client: Arc<dyn PraefectusClient>, timeout: Duration) -> Self {
        Self { client, timeout }
    }
}

#[async_trait]
impl WitnessVerifierPort for PraefectusClientWitnessVerifier {
    async fn verify(
        &self,
        witness: &VoiceprintWitness,
        escalation_key: &EscalationKey,
    ) -> Result<(), WitnessVerificationError> {
        let escalation_ref = key_to_escalation_ref(escalation_key);
        let operator = witness.operator;
        let result = tokio::time::timeout(
            self.timeout,
            self.client
                .verify_voiceprint_witness(witness, operator, &escalation_ref),
        )
        .await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(WitnessVerificationError {
                reason: format!("praefectus rejected witness: {e}"),
            }),
            Err(_) => Err(WitnessVerificationError {
                reason: format!("praefectus verify timed out after {:?}", self.timeout),
            }),
        }
    }
}

/// Convert an `EscalationKey` (the wire-protocol identifier carried
/// on `CatastrophicAck.key`) into the `EscalationRef` string the
/// `PraefectusClient` expects. The mapping is deterministic so a
/// re-verification of the same key always queries the same ref.
fn key_to_escalation_ref(key: &EscalationKey) -> EscalationRef {
    let s = match key {
        EscalationKey::InstructionAcknowledged { instruction_id } => {
            format!("instruction_ack:{instruction_id}")
        }
        EscalationKey::InstructionResult { instruction_id } => {
            format!("instruction_result:{instruction_id}")
        }
        EscalationKey::SessionBlocked {
            session_id,
            detected_at_ms,
        } => {
            format!("session_blocked:{session_id}:{detected_at_ms}")
        }
        EscalationKey::SessionCompleted {
            session_id,
            completed_at_ms,
        } => {
            format!("session_completed:{session_id}:{completed_at_ms}")
        }
        EscalationKey::SessionFailed {
            session_id,
            failed_at_ms,
        } => {
            format!("session_failed:{session_id}:{failed_at_ms}")
        }
    };
    EscalationRef(s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use chrono::Utc;
    use consul_domain::identity::republic::{ChallengeNonce, OperatorId};
    use consul_domain::identity::SessionId;
    use uuid::Uuid;

    use super::*;
    use crate::praefectus_client::InMemoryPraefectusClient;

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
    async fn verifier_accepts_when_inmemory_client_does() {
        let client = Arc::new(InMemoryPraefectusClient::new());
        let v = PraefectusClientWitnessVerifier::new(client);
        // InMemoryPraefectusClient defaults to accepting.
        let r = v.verify(&fixture_witness(), &fixture_key()).await;
        assert!(r.is_ok(), "expected Ok, got {r:?}");
    }

    #[tokio::test]
    async fn verifier_rejects_when_inmemory_client_does() {
        let client = InMemoryPraefectusClient::new();
        client.set_fail_verification(true);
        let v = PraefectusClientWitnessVerifier::new(Arc::new(client));
        let r = v.verify(&fixture_witness(), &fixture_key()).await;
        let err = r.unwrap_err();
        assert!(
            err.reason.contains("praefectus rejected"),
            "expected rejection reason, got: {err}"
        );
    }

    #[tokio::test]
    async fn verifier_times_out_on_slow_client() {
        // Custom client that sleeps longer than the verifier's
        // configured timeout.
        struct SlowClient;
        #[async_trait]
        impl PraefectusClient for SlowClient {
            async fn verify_voiceprint_witness(
                &self,
                _witness: &VoiceprintWitness,
                _operator: OperatorId,
                _escalation: &EscalationRef,
            ) -> Result<(), crate::praefectus_client::PraefectusClientError> {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(())
            }
            async fn current_role_binding(
                &self,
                _session_id: &str,
            ) -> Result<
                Option<consul_domain::identity::republic::RoleBinding>,
                crate::praefectus_client::PraefectusClientError,
            > {
                Ok(None)
            }
        }
        let v =
            PraefectusClientWitnessVerifier::with_timeout(Arc::new(SlowClient), Duration::from_millis(50));
        let r = v.verify(&fixture_witness(), &fixture_key()).await;
        let err = r.unwrap_err();
        assert!(err.reason.contains("timed out"), "expected timeout, got: {err}");
    }

    #[test]
    fn key_to_escalation_ref_is_deterministic() {
        let k1 = fixture_key();
        let k2 = fixture_key();
        assert_eq!(key_to_escalation_ref(&k1).0, key_to_escalation_ref(&k2).0);
    }

    #[test]
    fn key_to_escalation_ref_distinguishes_variants() {
        let session = SessionId::from_uuid(Uuid::from_bytes([0xBB; 16]));
        let a = EscalationKey::SessionBlocked {
            session_id: session,
            detected_at_ms: 1_700_000_000_000,
        };
        let b = EscalationKey::SessionCompleted {
            session_id: session,
            completed_at_ms: 1_700_000_000_000,
        };
        // Different variants with the same numeric stamp must
        // produce different refs (otherwise a Blocked-ack could
        // be confused with a Completed-ack).
        assert_ne!(key_to_escalation_ref(&a).0, key_to_escalation_ref(&b).0);
    }
}
