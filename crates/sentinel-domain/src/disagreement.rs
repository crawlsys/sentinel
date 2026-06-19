//! `DisagreementMarker` — the chain entry that records multi-judge
//! disagreement (Stage B Part 2 of #82, follow-up to commit 00b6e91
//! which shipped the `multi_judge` types).
//!
//! When a critical / critical-strict tier runs N judges in parallel
//! and they disagree on `sufficient`, the engine appends a
//! `DisagreementMarker` to the chain right after the `StepProof`
//! that triggered the disagreement. The chain reads:
//!
//! ```text
//! StepProof(step_id="3.L2.3") → DisagreementMarker(...) → next entry
//! ```
//!
//! The marker is a discrete, queryable signal — chain walkers find
//! disagreements by filtering for `ProofEntry::Disagreement`, no
//! per-step inspection needed.
//!
//! # Hash discipline
//!
//! Same shape as `PhaseProof` / `StepProof`:
//!
//! - `previous_hash` = the head hash before the marker (the
//!   triggering `StepProof`'s `combined_hash`)
//! - `multi_judge_hash` = SHA-256 over the canonical JSON of
//!   [`MultiJudgeVerdict`]. Tampering with the per-judge breakdown
//!   mid-chain breaks `verify_self`.
//! - `combined_hash` = SHA-256 over `("disagreement" || skill ||
//!   session_id || step_id || multi_judge_hash || previous_hash)`.
//!   The `"disagreement"` discriminator prevents a forger from
//!   substituting a `StepProof`'s `combined_hash` for a
//!   `DisagreementMarker`'s (and vice versa) — distinct hash domains
//!   per entry kind.
//! - `signature` = Ed25519 signature over `combined_hash`. Authoritative
//!   verification rejects unsigned markers.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::multi_judge::MultiJudgeVerdict;
use crate::step_proof::SignatureError;

/// A chain entry recording multi-judge disagreement on a step's
/// verdict.
///
/// The triggering `StepProof` appears in the chain as usual; this
/// marker is appended right after it so a chain walker can see the
/// per-judge breakdown without re-running the judges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisagreementMarker {
    /// Skill name (matches the `StepProof` this marker follows).
    pub skill: String,
    /// Session ID.
    pub session_id: String,
    /// Step the disagreement is about. Matches the triggering
    /// `StepProof`'s `step_id`.
    pub step_id: String,
    /// Phase the step lives under.
    pub phase_id: String,

    /// Full per-judge breakdown — every model's verdict + cost +
    /// (audit-grade) provider. The reason this marker exists.
    pub multi_judge: MultiJudgeVerdict,

    // ── Chain ────────────────────────────────────────────────────
    /// Previous chain head's `combined_hash`.
    pub previous_hash: String,
    /// SHA-256 over the canonical-serialised [`MultiJudgeVerdict`].
    /// Folded into `combined_hash` so tampering with per-judge
    /// fields breaks chain verification.
    pub multi_judge_hash: String,
    /// SHA-256 over (`"disagreement"` || skill || `session_id` ||
    /// `step_id` || `multi_judge_hash` || `previous_hash`). The
    /// "disagreement" discriminator gives this entry kind its own
    /// hash domain — a `StepProof`'s `combined_hash` can never collide
    /// with a `DisagreementMarker`'s `combined_hash` even if every
    /// other field happened to match.
    pub combined_hash: String,

    // ── Ed25519 attestation ───────────────────────────────────────
    /// Hex-encoded Ed25519 signature over `combined_hash`.
    ///
    /// Authoritative verification rejects `None`. The option exists only so
    /// unsigned marker material can deserialize and produce a precise
    /// `SignatureError::Missing` instead of hiding behind a parse failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    /// When the disagreement was recorded (RFC3339 UTC).
    pub recorded_at: DateTime<Utc>,
}

impl DisagreementMarker {
    /// Compute the multi-judge hash from the verdict bytes.
    /// Determinism depends on `serde_json` using `BTreeMap` (the
    /// default) for object ordering. If the `preserve_order` feature
    /// is ever enabled workspace-wide, this hash will become
    /// non-deterministic — same caveat as `PhaseProof::compute_evidence_hash`.
    pub fn compute_multi_judge_hash(verdict: &MultiJudgeVerdict) -> String {
        let json = serde_json::to_string(verdict).expect(
            "MultiJudgeVerdict serialisation should never fail — all fields are simple types",
        );
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Compute the combined hash with the `"disagreement"`
    /// discriminator that gives this entry kind its own hash domain.
    pub fn compute_combined_hash(
        skill: &str,
        session_id: &str,
        step_id: &str,
        multi_judge_hash: &str,
        previous_hash: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"disagreement|"); // entry-kind discriminator
        hasher.update(skill.as_bytes());
        hasher.update(b"|");
        hasher.update(session_id.as_bytes());
        hasher.update(b"|");
        hasher.update(step_id.as_bytes());
        hasher.update(b"|");
        hasher.update(multi_judge_hash.as_bytes());
        hasher.update(b"|");
        hasher.update(previous_hash.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Build a new marker following `previous_hash`. Computes both
    /// internal hashes from the verdict + chain head.
    #[must_use]
    pub fn new(
        skill: impl Into<String>,
        session_id: impl Into<String>,
        step_id: impl Into<String>,
        phase_id: impl Into<String>,
        multi_judge: MultiJudgeVerdict,
        previous_hash: impl Into<String>,
    ) -> Self {
        let skill = skill.into();
        let session_id = session_id.into();
        let step_id = step_id.into();
        let phase_id = phase_id.into();
        let previous_hash = previous_hash.into();
        let multi_judge_hash = Self::compute_multi_judge_hash(&multi_judge);
        let combined_hash = Self::compute_combined_hash(
            &skill,
            &session_id,
            &step_id,
            &multi_judge_hash,
            &previous_hash,
        );
        Self {
            skill,
            session_id,
            step_id,
            phase_id,
            multi_judge,
            previous_hash,
            multi_judge_hash,
            combined_hash,
            signature: None,
            recorded_at: Utc::now(),
        }
    }

    /// Sign this marker's `combined_hash` with an Ed25519 key.
    pub fn sign_with(&mut self, key: &SigningKey) {
        let signature: Signature = key.sign(self.combined_hash.as_bytes());
        self.signature = Some(hex::encode(signature.to_bytes()));
    }

    /// Verify this marker's signature against a public key.
    ///
    /// Returns:
    /// - `Ok(true)` — signature is present and valid for this `combined_hash`
    /// - `Err(SignatureError)` — signature is absent, malformed, wrong length,
    ///   or doesn't verify against the supplied key
    pub fn verify_signature(&self, key: &VerifyingKey) -> Result<bool, SignatureError> {
        let Some(sig_hex) = &self.signature else {
            return Err(SignatureError::Missing);
        };
        let sig_bytes = hex::decode(sig_hex).map_err(|_| SignatureError::InvalidEncoding)?;
        if sig_bytes.len() != ed25519_dalek::SIGNATURE_LENGTH {
            return Err(SignatureError::InvalidLength);
        }
        let mut sig_array = [0u8; ed25519_dalek::SIGNATURE_LENGTH];
        sig_array.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_array);
        key.verify(self.combined_hash.as_bytes(), &signature)
            .map(|()| true)
            .map_err(|_| SignatureError::VerificationFailed)
    }

    /// Verify both internal hashes match recomputed values. Used by
    /// `ProofChain::verify` when walking a chain.
    #[must_use]
    pub fn verify_self(&self) -> bool {
        let expected_mj = Self::compute_multi_judge_hash(&self.multi_judge);
        if self.multi_judge_hash != expected_mj {
            return false;
        }
        let expected_combined = Self::compute_combined_hash(
            &self.skill,
            &self.session_id,
            &self.step_id,
            &self.multi_judge_hash,
            &self.previous_hash,
        );
        self.combined_hash == expected_combined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{JudgeModel, JudgeVerdict};
    use crate::multi_judge::{JudgeRun, JudgeTrustTier, MultiJudgeVerdict};

    fn sample_disagreement() -> MultiJudgeVerdict {
        MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                JudgeRun {
                    model: JudgeModel::Kimi,
                    verdict: JudgeVerdict::pass(0.95, "ok"),
                    cost_usd: Some(0.0023),
                    provider: None,
                },
                JudgeRun {
                    model: JudgeModel::Sonnet,
                    verdict: JudgeVerdict::fail(0.40, "no", vec![]),
                    cost_usd: Some(0.0150),
                    provider: None,
                },
            ],
        )
    }

    #[test]
    fn new_marker_computes_consistent_hashes() {
        let v = sample_disagreement();
        let m = DisagreementMarker::new("linear", "sess-1", "claim.3", "claim", v, "abc123");
        assert!(m.verify_self());
        assert!(!m.combined_hash.is_empty());
        assert!(!m.multi_judge_hash.is_empty());
    }

    #[test]
    fn tamper_with_multi_judge_breaks_verify() {
        let v = sample_disagreement();
        let mut m = DisagreementMarker::new("linear", "sess-1", "claim.3", "claim", v, "abc123");
        // Mutate the captured verdict — multi_judge_hash no longer
        // matches the recomputed value.
        m.multi_judge.sufficient = !m.multi_judge.sufficient;
        assert!(!m.verify_self());
    }

    #[test]
    fn tamper_with_step_id_breaks_verify() {
        let v = sample_disagreement();
        let mut m = DisagreementMarker::new("linear", "sess-1", "claim.3", "claim", v, "abc123");
        m.step_id = "claim.999".to_string();
        // multi_judge_hash still matches but combined_hash doesn't.
        assert!(!m.verify_self());
    }

    #[test]
    fn discriminator_distinguishes_disagreement_from_step_hashes() {
        // A DisagreementMarker's combined_hash MUST differ from any
        // StepProof's combined_hash, even when (skill, session_id,
        // step_id, evidence_hash, previous_hash) coincide. The
        // "disagreement|" prefix in compute_combined_hash is the
        // load-bearing piece — pin it.
        let mj_hash = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let dis = DisagreementMarker::compute_combined_hash(
            "linear", "sess-1", "claim.3", mj_hash, "prev",
        );
        // What a StepProof would compute over the same six bytes
        // *without* the discriminator:
        let mut hasher = Sha256::new();
        hasher.update(b"linear|");
        hasher.update(b"sess-1|");
        hasher.update(b"claim.3|");
        hasher.update(mj_hash.as_bytes());
        hasher.update(b"|prev");
        let naked: String = format!("{:x}", hasher.finalize());
        assert_ne!(
            dis, naked,
            "discriminator must produce a different hash domain",
        );
    }

    #[test]
    fn same_inputs_produce_stable_combined_hash() {
        // Determinism: same inputs → same hash. Required for verify
        // and for cross-machine chain replay.
        let mj_hash = "abc";
        let h1 = DisagreementMarker::compute_combined_hash(
            "linear", "sess-1", "claim.3", mj_hash, "prev",
        );
        let h2 = DisagreementMarker::compute_combined_hash(
            "linear", "sess-1", "claim.3", mj_hash, "prev",
        );
        assert_eq!(h1, h2);
    }

    #[test]
    fn marker_serde_round_trip() {
        let v = sample_disagreement();
        let m = DisagreementMarker::new("linear", "sess-1", "claim.3", "claim", v, "abc123");
        let json = serde_json::to_string(&m).unwrap();
        let back: DisagreementMarker = serde_json::from_str(&json).unwrap();
        assert!(back.verify_self());
        assert_eq!(back.skill, "linear");
        assert_eq!(back.step_id, "claim.3");
        assert_eq!(back.multi_judge.tier, JudgeTrustTier::Critical);
        assert!(back.multi_judge.disagreement);
    }

    #[test]
    fn sign_then_verify_succeeds_with_matching_key() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = key.verifying_key();
        let v = sample_disagreement();
        let mut m = DisagreementMarker::new("linear", "sess-1", "claim.3", "claim", v, "abc123");

        assert!(m.signature.is_none(), "fresh marker has no signature");
        m.sign_with(&key);
        assert!(m.signature.is_some(), "sign_with must populate signature");
        assert!(
            m.verify_signature(&public)
                .expect("valid marker signature should verify"),
            "valid marker signature must verify"
        );
    }

    #[test]
    fn unsigned_marker_signature_verification_fails_closed() {
        let public = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        let v = sample_disagreement();
        let m = DisagreementMarker::new("linear", "sess-1", "claim.3", "claim", v, "abc123");

        assert_eq!(
            m.verify_signature(&public),
            Err(SignatureError::Missing),
            "unsigned disagreement markers must fail authoritative verification"
        );
    }

    #[test]
    fn multi_judge_hash_changes_with_per_judge_changes() {
        // Two markers, identical except one judge's confidence
        // value. The multi_judge_hash must differ — otherwise a
        // forger could swap in a higher-confidence verdict
        // post-hoc without breaking the chain.
        let v1 = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![JudgeRun {
                model: JudgeModel::Kimi,
                verdict: JudgeVerdict::fail(0.20, "no", vec![]),
                cost_usd: None,
                provider: None,
            }],
        );
        let v2 = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![JudgeRun {
                model: JudgeModel::Kimi,
                verdict: JudgeVerdict::fail(0.99, "no", vec![]),
                cost_usd: None,
                provider: None,
            }],
        );
        let h1 = DisagreementMarker::compute_multi_judge_hash(&v1);
        let h2 = DisagreementMarker::compute_multi_judge_hash(&v2);
        assert_ne!(h1, h2);
    }
}
