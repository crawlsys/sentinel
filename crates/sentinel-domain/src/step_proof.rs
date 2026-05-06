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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence::Evidence;
use crate::judge::JudgeVerdict;

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
        let mut p = make_step("1", "claim", "linear", GENESIS_HASH, serde_json::Value::Null);
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
        let mut p = make_step("1", "claim", "linear", GENESIS_HASH, serde_json::Value::Null);
        p.combined_hash = "0".repeat(64);
        assert!(!p.verify_self());
    }

    #[test]
    fn verify_self_rejects_backwards_timestamps() {
        let mut p = make_step("1", "claim", "linear", GENESIS_HASH, serde_json::Value::Null);
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
        let mut p = make_step("1", "claim", "linear", GENESIS_HASH, serde_json::Value::Null);
        p.account_context = Some("firefly-pro".into());
        let json = serde_json::to_string(&p).expect("serialize");
        let back: StepProof = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.account_context.as_deref(), Some("firefly-pro"));
    }

    #[test]
    fn account_context_omitted_when_none() {
        let p = make_step("1", "claim", "linear", GENESIS_HASH, serde_json::Value::Null);
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("account_context"),
            "None account_context must be skipped during serialization, got: {json}",
        );
    }

    #[test]
    fn signature_round_trips_through_serde() {
        let mut p = make_step("1", "claim", "linear", GENESIS_HASH, serde_json::Value::Null);
        p.signature = Some("abcd1234".into());
        let json = serde_json::to_string(&p).expect("serialize");
        let back: StepProof = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.signature.as_deref(), Some("abcd1234"));
    }
}
