//! Step-Level Proof of Work
//!
//! Cryptographic proof for a single step within a phase. Modeled on
//! [`PhaseProof`](crate::proof::PhaseProof), but at finer granularity:
//! every step in `config/steps/<skill>.toml` produces one `StepProof` when
//! it completes. Step proofs and phase proofs share a single hash chain via
//! [`ProofEntry`](crate::proof::ProofEntry) so a chain can interleave coarse
//! phase boundaries and fine step boundaries without breaking continuity.
//!
//! # Trust model (mirrors PhaseProof)
//!
//! Each step proof commits to:
//! - the step identity (`skill`, `phase_id`, `step_id`)
//! - an evidence blob (whatever the step produced — tool input/output, files
//!   touched, exit codes) hashed via SHA-256
//! - the previous chain head's `combined_hash` (or [`GENESIS_HASH`])
//! - an artifact JSON value — the *typed handoff* that downstream steps
//!   consume as input. This is the "Apollo `@key`" of our federation: the
//!   chain of typed StepProof artifacts is the chain of trust between
//!   composed steps.
//!
//! # Step-specific extensions over PhaseProof
//!
//! - `artifact`: arbitrary JSON value carrying the typed handoff. Empty by
//!   default (e.g. for steps that only verify state and produce no payload).
//! - `account_context`: optional account/tenant scope (Linear workspace,
//!   Doppler config, Auth0 tenant) — propagates with the chain so cross-skill
//!   handoffs preserve tenancy. Apollo header-propagation analog.
//! - `signature`: optional Ed25519 signature over the combined hash. Set
//!   when `SENTINEL_SIGNING_KEY` is configured. Mandatory chain integrity
//!   stays via SHA-256; signing is the AEGIS-borrowed enterprise opt-in.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence::Evidence;
use crate::judge::JudgeVerdict;
use crate::tracing::TraceContext;

/// A single step's proof of work.
///
/// Shape parallels [`PhaseProof`](crate::proof::PhaseProof) so callers and
/// the [`ProofChain`](crate::proof::ProofChain) verifier can treat the two
/// uniformly via [`ProofEntry`](crate::proof::ProofEntry). The combined hash
/// includes the artifact bytes so downstream consumers can trust the typed
/// handoff has not been tampered with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepProof {
    /// Step identifier (e.g. "3.L2.3", "claim.1") — globally unique within
    /// a `(skill, phase_id)` pair, scoped per the config TOML.
    pub step_id: String,

    /// Phase this step lives under (e.g. "claim", "review"). Combined with
    /// `step_id` to disambiguate steps that share an `id` across phases.
    pub phase_id: String,

    /// Skill this step belongs to (e.g. "linear", "git", "deploy").
    pub skill: String,

    /// Session ID for the run that produced this proof.
    pub session_id: String,

    // ── Evidence ──
    /// Collected evidence for this step (tool inputs, outputs, files, etc).
    pub evidence: Evidence,

    /// SHA-256 of the serialized evidence — same algorithm as PhaseProof.
    pub evidence_hash: String,

    // ── Apollo Federation: typed handoff artifact ──
    /// Typed handoff payload for the next step in the chain. Free-form JSON
    /// at the type level; the federation compose CLI validates that producer
    /// and consumer agree on shape. `Value::Null` for steps that produce no
    /// downstream input (verification-only steps).
    #[serde(default)]
    pub artifact: serde_json::Value,

    /// SHA-256 of the canonical-serialized artifact. Folded into the
    /// combined hash so artifact tampering breaks the chain. Empty string
    /// when artifact is `Null` (no payload to hash).
    pub artifact_hash: String,

    // ── Header propagation (Apollo pattern) ──
    /// Account / tenant context that should propagate through the chain.
    /// Examples: `Some("firefly-pro")` for Linear workspace,
    /// `Some("dev")` for Doppler config, `Some("staging")` for Auth0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_context: Option<String>,

    // ── Chain ──
    /// Previous chain head's `combined_hash` (or [`GENESIS_HASH`] for the
    /// first entry). Whether the prior entry was a [`PhaseProof`] or another
    /// [`StepProof`] doesn't matter — the chain is a sequence of opaque
    /// 32-byte hashes.
    pub previous_hash: String,

    /// SHA-256 over (step_id || phase_id || skill || evidence_hash ||
    /// artifact_hash || previous_hash). The "tessera" — what the next entry
    /// will reference as its `previous_hash`.
    pub combined_hash: String,

    // ── AI Judge ──
    /// Which model judged this step's evidence (e.g. "sonnet-4.6", "opus-4.7").
    pub judge_model: String,

    /// The judge's verdict — sufficient/insufficient + reasoning.
    pub judge_verdict: JudgeVerdict,

    // ── Optional Ed25519 signing (AEGIS pattern, opt-in) ──
    /// Hex-encoded Ed25519 signature over `combined_hash` when signing is
    /// enabled via `SENTINEL_SIGNING_KEY`. Verified by `verify_self` only
    /// when present; absence is not a failure (same hash chain integrity
    /// applies either way). The companion public key is recorded in the
    /// chain's metadata so verifiers know which key to expect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    // ── Optional OpenTelemetry trace linkage (M4.5) ──
    /// W3C trace context the step was emitted under. `None` when OTEL
    /// isn't configured (the common case until the OTLP exporter lands).
    /// `Some(ctx)` once tracing is wired up so a corpus query can pivot
    /// directly to Grafana / Tempo / Honeycomb.
    ///
    /// **Not included in the combined hash.** Trace context is
    /// operational metadata, not part of the audit contract — adding
    /// it to the hash would mean a chain emitted with OTEL on couldn't
    /// be verified by a sentinel build with OTEL off, which defeats the
    /// purpose. The audit story is the proof chain; OTEL is alongside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<TraceContext>,

    // ── Metadata ──
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: u64,
}

impl StepProof {
    /// Compute the evidence hash from evidence data.
    ///
    /// Determinism note (mirrors PhaseProof): relies on `serde_json`'s default
    /// `BTreeMap` object key ordering. Enabling the `preserve_order` feature
    /// would break cross-machine hash agreement.
    pub fn compute_evidence_hash(evidence: &Evidence) -> String {
        let json = serde_json::to_string(evidence)
            .expect("Evidence serialization should never fail — all fields are simple types");
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Compute the artifact hash from the handoff JSON value.
    ///
    /// Returns the empty string for `Value::Null` so verification-only steps
    /// (no payload) stay backward-compatible with simpler chains. Otherwise
    /// hashes the canonical JSON representation.
    #[must_use]
    pub fn compute_artifact_hash(artifact: &serde_json::Value) -> String {
        if artifact.is_null() {
            return String::new();
        }
        let json = serde_json::to_string(artifact)
            .expect("Artifact serialization should never fail — Value is always serializable");
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Compute the combined hash (the tessera).
    ///
    /// Order is fixed: `step_id || phase_id || skill || evidence_hash ||
    /// artifact_hash || previous_hash`. Changing the order would invalidate
    /// every existing chain on disk; do not reorder without a migration.
    #[must_use]
    pub fn compute_combined_hash(
        step_id: &str,
        phase_id: &str,
        skill: &str,
        evidence_hash: &str,
        artifact_hash: &str,
        previous_hash: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(step_id.as_bytes());
        hasher.update(phase_id.as_bytes());
        hasher.update(skill.as_bytes());
        hasher.update(evidence_hash.as_bytes());
        hasher.update(artifact_hash.as_bytes());
        hasher.update(previous_hash.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Sign this proof's `combined_hash` with an Ed25519 key (M1.7 — AEGIS pattern).
    ///
    /// Mutates `self.signature` to contain the hex-encoded 64-byte
    /// Ed25519 signature over the bytes of `combined_hash`. Idempotent:
    /// signing a proof that's already signed re-signs it (the signature
    /// is a deterministic function of the key + combined_hash).
    ///
    /// **Mandatory chain integrity stays SHA-256.** Signing is the
    /// enterprise compliance opt-in: when `SENTINEL_SIGNING_KEY` is
    /// configured upstream, every StepProof gets signed at write time,
    /// and verifiers can confirm "this chain entry was authored by the
    /// holder of <public_key>" — closing the residual "did sentinel
    /// really write this?" question that hash-only chains can't answer.
    ///
    /// Caller owns the key material. sentinel-domain stays pure — no
    /// env reads, no key management. Infrastructure / CLI / hooks
    /// load the key, sentinel-domain does the math.
    pub fn sign_with(&mut self, key: &SigningKey) {
        let signature: Signature = key.sign(self.combined_hash.as_bytes());
        self.signature = Some(hex::encode(signature.to_bytes()));
    }

    /// Verify this proof's signature (if present) against a public key.
    ///
    /// Returns:
    /// - `Ok(true)` — signature is present and valid for this `combined_hash`
    /// - `Ok(false)` — signature is absent (chain entry was written
    ///   without signing enabled — not a failure, just unsigned)
    /// - `Err(SignatureError)` — signature is present but malformed,
    ///   wrong length, or doesn't verify against the supplied key
    ///
    /// The 3-way return is deliberate: callers walking a chain need
    /// to distinguish "unsigned by policy" from "signed but tampered."
    /// Treating absence as failure would break backwards compat with
    /// existing chains that pre-date M1.7.
    pub fn verify_signature(&self, key: &VerifyingKey) -> Result<bool, SignatureError> {
        let Some(sig_hex) = &self.signature else {
            return Ok(false);
        };
        let sig_bytes = hex::decode(sig_hex).map_err(|_| SignatureError::InvalidEncoding)?;
        if sig_bytes.len() != ed25519_dalek::SIGNATURE_LENGTH {
            return Err(SignatureError::InvalidLength);
        }
        let mut sig_array = [0u8; ed25519_dalek::SIGNATURE_LENGTH];
        sig_array.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_array);
        key.verify(self.combined_hash.as_bytes(), &signature)
            .map(|_| true)
            .map_err(|_| SignatureError::VerificationFailed)
    }

    /// Verify this proof's hashes and timestamps are internally consistent.
    ///
    /// Returns false if any of the following are wrong:
    /// - `completed_at < started_at` (Attack #170 parity — backwards time)
    /// - `evidence_hash` doesn't match recomputed evidence hash
    /// - `artifact_hash` doesn't match recomputed artifact hash
    /// - `combined_hash` doesn't match recomputed combined hash
    ///
    /// Signature verification is intentionally NOT here — that requires the
    /// chain's public key, which lives at the chain level not the step level.
    /// Signature checks happen during full-chain verification.
    #[must_use]
    pub fn verify_self(&self) -> bool {
        // Temporal ordering — completed_at must be at-or-after started_at.
        if self.completed_at < self.started_at {
            return false;
        }

        // Evidence hash recomputation.
        let expected_evidence = Self::compute_evidence_hash(&self.evidence);
        if self.evidence_hash != expected_evidence {
            return false;
        }

        // Artifact hash recomputation.
        let expected_artifact = Self::compute_artifact_hash(&self.artifact);
        if self.artifact_hash != expected_artifact {
            return false;
        }

        // Combined hash — the tessera.
        let expected_combined = Self::compute_combined_hash(
            &self.step_id,
            &self.phase_id,
            &self.skill,
            &self.evidence_hash,
            &self.artifact_hash,
            &self.previous_hash,
        );
        self.combined_hash == expected_combined
    }
}

/// Errors from [`StepProof::verify_signature`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// Signature field is not valid hex.
    InvalidEncoding,
    /// Signature is hex-decoded but not 64 bytes (Ed25519 length).
    InvalidLength,
    /// Signature decodes correctly but doesn't verify against the
    /// supplied public key for this proof's `combined_hash`. Indicates
    /// either tampering OR wrong public key.
    VerificationFailed,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEncoding => write!(f, "signature is not valid hex"),
            Self::InvalidLength => write!(f, "signature is not 64 bytes (Ed25519 length)"),
            Self::VerificationFailed => write!(
                f,
                "signature does not verify against the supplied public key"
            ),
        }
    }
}

impl std::error::Error for SignatureError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::GENESIS_HASH;

    fn make_step(
        step_id: &str,
        phase_id: &str,
        skill: &str,
        previous_hash: &str,
        artifact: serde_json::Value,
    ) -> StepProof {
        let evidence = Evidence::default();
        let evidence_hash = StepProof::compute_evidence_hash(&evidence);
        let artifact_hash = StepProof::compute_artifact_hash(&artifact);
        let combined_hash = StepProof::compute_combined_hash(
            step_id,
            phase_id,
            skill,
            &evidence_hash,
            &artifact_hash,
            previous_hash,
        );
        StepProof {
            step_id: step_id.into(),
            phase_id: phase_id.into(),
            skill: skill.into(),
            session_id: "test-session".into(),
            evidence,
            evidence_hash,
            artifact,
            artifact_hash,
            account_context: None,
            previous_hash: previous_hash.into(),
            combined_hash,
            judge_model: "sonnet-4.6".into(),
            judge_verdict: JudgeVerdict {
                sufficient: true,
                confidence: 0.95,
                reasoning: "ok".into(),
                requested_evidence: None,
            },
            signature: None,
            trace_context: None,
            started_at: Utc::now(),
            completed_at: Utc::now(),
            duration_ms: 42,
        }
    }

    #[test]
    fn evidence_hash_is_deterministic() {
        let e = Evidence::default();
        assert_eq!(
            StepProof::compute_evidence_hash(&e),
            StepProof::compute_evidence_hash(&e),
        );
    }

    #[test]
    fn artifact_hash_null_is_empty() {
        let h = StepProof::compute_artifact_hash(&serde_json::Value::Null);
        assert!(h.is_empty(), "Null artifact must hash to empty string");
    }

    #[test]
    fn artifact_hash_distinguishes_payloads() {
        let a = serde_json::json!({"pr_url": "https://github.com/foo/bar/pull/1"});
        let b = serde_json::json!({"pr_url": "https://github.com/foo/bar/pull/2"});
        assert_ne!(
            StepProof::compute_artifact_hash(&a),
            StepProof::compute_artifact_hash(&b),
        );
    }

    #[test]
    fn combined_hash_changes_with_any_input() {
        let base = StepProof::compute_combined_hash("s1", "p1", "linear", "eh", "ah", GENESIS_HASH);
        // Each component change should yield a different hash.
        assert_ne!(
            base,
            StepProof::compute_combined_hash("s2", "p1", "linear", "eh", "ah", GENESIS_HASH),
        );
        assert_ne!(
            base,
            StepProof::compute_combined_hash("s1", "p2", "linear", "eh", "ah", GENESIS_HASH),
        );
        assert_ne!(
            base,
            StepProof::compute_combined_hash("s1", "p1", "git", "eh", "ah", GENESIS_HASH),
        );
        assert_ne!(
            base,
            StepProof::compute_combined_hash("s1", "p1", "linear", "eh2", "ah", GENESIS_HASH),
        );
        assert_ne!(
            base,
            StepProof::compute_combined_hash("s1", "p1", "linear", "eh", "ah2", GENESIS_HASH),
        );
        assert_ne!(
            base,
            StepProof::compute_combined_hash("s1", "p1", "linear", "eh", "ah", "different"),
        );
    }

    #[test]
    fn verify_self_accepts_well_formed_proof() {
        let p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::json!({"ticket_id": "FPCRM-1"}),
        );
        assert!(p.verify_self());
    }

    #[test]
    fn verify_self_rejects_evidence_tamper() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        // Mutate evidence after the hash was sealed.
        p.evidence.phase_file_read = !p.evidence.phase_file_read;
        assert!(!p.verify_self());
    }

    #[test]
    fn verify_self_rejects_artifact_tamper() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::json!({"pr_url": "x"}),
        );
        // Mutate the artifact post-seal — combined hash and artifact hash
        // both still reference the *original* payload.
        p.artifact = serde_json::json!({"pr_url": "y"});
        assert!(!p.verify_self());
    }

    #[test]
    fn verify_self_rejects_combined_hash_tamper() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.combined_hash = "0".repeat(64);
        assert!(!p.verify_self());
    }

    #[test]
    fn verify_self_rejects_backwards_timestamps() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        let now = Utc::now();
        p.started_at = now;
        p.completed_at = now - chrono::Duration::seconds(1);
        // Re-seal hashes so only the timestamp ordering is wrong.
        p.evidence_hash = StepProof::compute_evidence_hash(&p.evidence);
        p.artifact_hash = StepProof::compute_artifact_hash(&p.artifact);
        p.combined_hash = StepProof::compute_combined_hash(
            &p.step_id,
            &p.phase_id,
            &p.skill,
            &p.evidence_hash,
            &p.artifact_hash,
            &p.previous_hash,
        );
        assert!(!p.verify_self(), "completed_at < started_at must fail");
    }

    #[test]
    fn account_context_round_trips_through_serde() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.account_context = Some("firefly-pro".into());
        let json = serde_json::to_string(&p).expect("serialize");
        let back: StepProof = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.account_context.as_deref(), Some("firefly-pro"));
    }

    #[test]
    fn account_context_omitted_when_none() {
        let p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("account_context"),
            "None account_context must be skipped during serialization, got: {json}",
        );
    }

    #[test]
    fn signature_round_trips_through_serde() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.signature = Some("abcd1234".into());
        let json = serde_json::to_string(&p).expect("serialize");
        let back: StepProof = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.signature.as_deref(), Some("abcd1234"));
    }

    // ── Ed25519 signing (M1.7 — AEGIS pattern) ─────────────────────────

    fn make_signing_key() -> SigningKey {
        // Deterministic key for tests — never use this seed in real code.
        let seed = [42u8; 32];
        SigningKey::from_bytes(&seed)
    }

    #[test]
    fn sign_with_populates_signature_field() {
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        assert!(p.signature.is_none(), "fresh proof has no signature");
        p.sign_with(&make_signing_key());
        assert!(p.signature.is_some(), "sign_with must populate signature");
        // Hex-encoded Ed25519 sig is 128 hex chars (64 bytes * 2).
        assert_eq!(p.signature.as_ref().unwrap().len(), 128);
    }

    #[test]
    fn sign_then_verify_succeeds_with_matching_key() {
        let key = make_signing_key();
        let public = key.verifying_key();
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.sign_with(&key);
        let result = p
            .verify_signature(&public)
            .expect("verify should not error");
        assert!(result, "valid signature must verify true");
    }

    #[test]
    fn verify_returns_false_for_unsigned_proof() {
        // Backwards compat: chains written before M1.7 have no signature.
        // verify_signature must distinguish "unsigned by policy" from
        // "signed but tampered" — return Ok(false), not Err.
        let p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        let public = make_signing_key().verifying_key();
        let result = p
            .verify_signature(&public)
            .expect("unsigned must not error");
        assert!(!result, "unsigned proof returns Ok(false)");
    }

    #[test]
    fn verify_fails_with_wrong_public_key() {
        let key_a = make_signing_key();
        let key_b = SigningKey::from_bytes(&[7u8; 32]);
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.sign_with(&key_a);
        // Verify against B's public key — must fail.
        let result = p.verify_signature(&key_b.verifying_key());
        assert_eq!(result, Err(SignatureError::VerificationFailed));
    }

    #[test]
    fn verify_fails_when_combined_hash_was_tampered_after_signing() {
        // Signature commits to combined_hash. Mutating combined_hash
        // after signing must break verification — that's the whole
        // point of signing layered on top of hash chaining.
        let key = make_signing_key();
        let public = key.verifying_key();
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.sign_with(&key);
        p.combined_hash = "0".repeat(64); // tamper
        let result = p.verify_signature(&public);
        assert_eq!(result, Err(SignatureError::VerificationFailed));
    }

    #[test]
    fn verify_errors_on_malformed_signature_hex() {
        let key = make_signing_key();
        let public = key.verifying_key();
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.signature = Some("not-valid-hex!!".into());
        let result = p.verify_signature(&public);
        assert_eq!(result, Err(SignatureError::InvalidEncoding));
    }

    #[test]
    fn verify_errors_on_wrong_length_signature() {
        let key = make_signing_key();
        let public = key.verifying_key();
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        // Hex-valid but only 32 bytes (half the required length).
        p.signature = Some("ab".repeat(32));
        let result = p.verify_signature(&public);
        assert_eq!(result, Err(SignatureError::InvalidLength));
    }

    #[test]
    fn signature_persists_through_serde_round_trip() {
        let key = make_signing_key();
        let public = key.verifying_key();
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.sign_with(&key);

        // Round trip through JSON.
        let json = serde_json::to_string(&p).unwrap();
        let restored: StepProof = serde_json::from_str(&json).unwrap();

        // Restored proof must still verify against the same key.
        assert!(restored
            .verify_signature(&public)
            .expect("verify post-round-trip"));
    }

    // ─── M4.5 trace_context tests ────────────────────────────────────

    #[test]
    fn trace_context_defaults_to_none_on_new_proof() {
        // Fresh StepProofs constructed via make_step have no trace
        // context — that's the OTEL-off baseline. Existing chains
        // serialized before M4.5 deserialize with this same shape.
        let p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        assert!(p.trace_context.is_none());
    }

    #[test]
    fn trace_context_is_skipped_when_none_in_serialized_form() {
        // skip_serializing_if = "Option::is_none" — chains written
        // with OTEL off must be byte-identical to pre-M4.5 chains.
        // If this regresses, every chain on disk gets a `null`
        // field appended and verification breaks subtly.
        let p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("trace_context"),
            "None trace_context must not appear in JSON, got: {json}",
        );
    }

    #[test]
    fn trace_context_round_trips_through_serde() {
        // When OTEL is on, the field is Some(ctx) and serialization
        // must preserve every sub-field including parent_span_id and
        // tracestate.
        use crate::tracing::{TraceContext, FLAG_SAMPLED};
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.trace_context = Some(TraceContext {
            trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            span_id: "b7ad6b7169203331".into(),
            parent_span_id: Some("a1b2c3d4e5f60789".into()),
            flags: FLAG_SAMPLED,
            tracestate: vec![("vendor".into(), "value".into())],
        });

        let json = serde_json::to_string(&p).unwrap();
        let restored: StepProof = serde_json::from_str(&json).unwrap();
        let ctx = restored.trace_context.expect("trace_context preserved");
        assert_eq!(ctx.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(ctx.span_id, "b7ad6b7169203331");
        assert_eq!(ctx.parent_span_id.as_deref(), Some("a1b2c3d4e5f60789"));
        assert!(ctx.is_sampled());
        assert_eq!(ctx.tracestate.len(), 1);
    }

    #[test]
    fn trace_context_does_not_affect_combined_hash() {
        // The audit contract: trace_context is operational metadata,
        // not part of the chain hash. Two proofs identical in every
        // proof-relevant field but differing only in trace_context
        // must hash to the same combined_hash. Otherwise OTEL-on
        // sentinel and OTEL-off sentinel can't share chains.
        use crate::tracing::TraceContext;
        let p1 = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        let mut p2 = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p2.trace_context = Some(TraceContext::new_root(
            "0af7651916cd43dd8448eb211c80319c",
            "b7ad6b7169203331",
        ));
        // Both proofs were built with the same compute_combined_hash
        // inputs — the hash must be identical regardless of
        // trace_context state.
        assert_eq!(
            p1.combined_hash, p2.combined_hash,
            "trace_context must not be folded into combined_hash",
        );
    }

    #[test]
    fn trace_context_does_not_affect_signature_verification() {
        // Signing covers combined_hash only; trace_context lives
        // outside the signed envelope. A proof signed with OTEL off
        // must verify cleanly even after a tracer attaches a
        // trace_context post-hoc (e.g. for a corpus migration).
        use crate::tracing::TraceContext;
        let key = make_signing_key();
        let public = key.verifying_key();
        let mut p = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        p.sign_with(&key);
        assert!(p.verify_signature(&public).unwrap());

        // Attach trace_context after signing — verification still passes.
        p.trace_context = Some(TraceContext::new_root(
            "0af7651916cd43dd8448eb211c80319c",
            "b7ad6b7169203331",
        ));
        assert!(
            p.verify_signature(&public).unwrap(),
            "post-hoc trace_context attachment must not invalidate the signature",
        );
    }
}
