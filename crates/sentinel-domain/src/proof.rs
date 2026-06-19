//! Proof-of-Work Chain
//!
//! Cryptographic proof chain for skill phase execution.
//! Each phase produces a hash from (`phase_id` + `evidence_hash` + `previous_hash`),
//! creating a tamper-evident chain — same trust model as blockchain.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence::Evidence;
use crate::judge::JudgeVerdict;
use crate::step_proof::{SignatureError, StepProof};

/// Genesis hash — the "previous hash" for the first phase in a chain
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A single phase's proof of work
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseProof {
    /// Phase identifier (e.g., "claim", "fetch")
    pub phase_id: String,

    /// Skill this phase belongs to
    pub skill: String,

    /// Session ID
    pub session_id: String,

    // ── Evidence ──
    /// Collected evidence for this phase
    pub evidence: Evidence,

    /// SHA-256 hash of the serialized evidence
    pub evidence_hash: String,

    // ── Chain ──
    /// Previous phase's `combined_hash` (or `GENESIS_HASH` for first phase)
    pub previous_hash: String,

    /// SHA-256(phase_id + `evidence_hash` + `previous_hash`) — the tessera
    pub combined_hash: String,

    // ── AI Judge ──
    /// Which model judged this phase
    pub judge_model: String,

    /// The judge's verdict
    pub judge_verdict: JudgeVerdict,

    // ── Ed25519 attestation ──
    /// Hex-encoded Ed25519 signature over `combined_hash`.
    ///
    /// Authoritative verification rejects `None`. The option exists only so
    /// unsigned proof material can deserialize and produce a precise
    /// `SignatureError::Missing` instead of hiding behind a parse failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    // ── Metadata ──
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: u64,
}

impl PhaseProof {
    /// Compute the evidence hash from evidence data.
    ///
    /// Note: Evidence hash determinism depends on `serde_json` using `BTreeMap`
    /// (the default) for JSON object ordering. If the `preserve_order` feature
    /// is enabled, ordering may change.
    pub fn compute_evidence_hash(evidence: &Evidence) -> String {
        let json = serde_json::to_string(evidence)
            .expect("Evidence serialization should never fail — all fields are simple types");
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Compute the combined hash (the tessera).
    ///
    /// Binds `judge_sufficient` — the AI judge's gate outcome — into the hash.
    /// The Ed25519 signature (when present) covers `combined_hash`, so without
    /// folding the verdict an attacker could flip `judge_verdict.sufficient` on a
    /// sealed/signed proof and it would still pass `verify_self` AND signature
    /// verification (the gate outcome was not integrity-protected). Folding it
    /// (domain-separated) makes the verdict tamper-evident.
    pub fn compute_combined_hash(
        phase_id: &str,
        skill: &str,
        evidence_hash: &str,
        previous_hash: &str,
        judge_sufficient: bool,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(phase_id.as_bytes());
        // Bind `skill` (as StepProof already does) so a proof can't be replayed
        // under a different skill's chain without breaking the hash.
        hasher.update(skill.as_bytes());
        hasher.update(evidence_hash.as_bytes());
        hasher.update(previous_hash.as_bytes());
        hasher.update(b"judge");
        hasher.update([u8::from(judge_sufficient)]);
        format!("{:x}", hasher.finalize())
    }

    /// Sign this proof's `combined_hash` with an Ed25519 key.
    pub fn sign_with(&mut self, key: &SigningKey) {
        let signature: Signature = key.sign(self.combined_hash.as_bytes());
        self.signature = Some(hex::encode(signature.to_bytes()));
    }

    /// Verify this proof's signature against a public key.
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

    /// Verify this proof's hashes and timestamps are internally consistent
    #[must_use]
    pub fn verify_self(&self) -> bool {
        // **Attack #170 fix**: Validate temporal ordering.
        // completed_at must be >= started_at. Without this, an attacker can craft
        // proofs with nonsensical timestamps (completed before started) that still
        // pass hash verification, confusing audit trails and AI judges.
        if self.completed_at < self.started_at {
            return false;
        }

        // Verify evidence hash
        let expected_evidence = Self::compute_evidence_hash(&self.evidence);
        if self.evidence_hash != expected_evidence {
            return false;
        }

        // Verify combined hash — includes the bound judge verdict, so a flipped
        // verdict on a sealed proof breaks recomputation.
        let expected_combined = Self::compute_combined_hash(
            &self.phase_id,
            &self.skill,
            &self.evidence_hash,
            &self.previous_hash,
            self.judge_verdict.sufficient,
        );
        self.combined_hash == expected_combined
    }
}

/// A single ordered entry in a proof chain — either a phase boundary or a
/// step within a phase. Both variants carry a `combined_hash` that links the
/// chain together; the verifier doesn't care which kind a given entry is, it
/// just walks `previous_hash` → `combined_hash` continuity.
///
/// Tagged serde representation so on-disk chains stay self-describing:
/// `{"kind":"phase",...}`, `{"kind":"step",...}`, or `{"kind":"disagreement",...}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
// Variants differ in size, but boxing would change every construction/match
// site of this on-disk-serialized enum for no runtime gain (entries are owned,
// not passed by value on a hot path).
#[allow(clippy::large_enum_variant)]
pub enum ProofEntry {
    Phase(PhaseProof),
    Step(StepProof),
    /// Multi-judge disagreement marker (#82 Stage B). Appended right
    /// after the `StepProof` that triggered the disagreement so chain
    /// walkers can find disagreements by filtering on the variant.
    Disagreement(crate::disagreement::DisagreementMarker),
}

impl ProofEntry {
    /// The hash that the next entry must reference as its `previous_hash`.
    #[must_use]
    pub fn combined_hash(&self) -> &str {
        match self {
            Self::Phase(p) => &p.combined_hash,
            Self::Step(s) => &s.combined_hash,
            Self::Disagreement(d) => &d.combined_hash,
        }
    }

    /// The `previous_hash` field — what this entry claims to follow.
    #[must_use]
    pub fn previous_hash(&self) -> &str {
        match self {
            Self::Phase(p) => &p.previous_hash,
            Self::Step(s) => &s.previous_hash,
            Self::Disagreement(d) => &d.previous_hash,
        }
    }

    /// Internal-consistency check (hashes match recomputed values).
    #[must_use]
    pub fn verify_self(&self) -> bool {
        match self {
            Self::Phase(p) => p.verify_self(),
            Self::Step(s) => s.verify_self(),
            Self::Disagreement(d) => d.verify_self(),
        }
    }

    /// Identifier suitable for error messages.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Phase(p) => p.phase_id.clone(),
            Self::Step(s) => format!("{}.{}", s.phase_id, s.step_id),
            Self::Disagreement(d) => format!("disagreement({}.{})", d.phase_id, d.step_id),
        }
    }

    /// `started_at` timestamp for cross-entry temporal ordering checks.
    /// `Disagreement` markers use `recorded_at` for both — they're a
    /// point-in-time annotation, not a span.
    #[must_use]
    pub const fn started_at(&self) -> DateTime<Utc> {
        match self {
            Self::Phase(p) => p.started_at,
            Self::Step(s) => s.started_at,
            Self::Disagreement(d) => d.recorded_at,
        }
    }

    /// `completed_at` timestamp for cross-entry temporal ordering checks.
    #[must_use]
    pub const fn completed_at(&self) -> DateTime<Utc> {
        match self {
            Self::Phase(p) => p.completed_at,
            Self::Step(s) => s.completed_at,
            Self::Disagreement(d) => d.recorded_at,
        }
    }
}

/// The full chain for a skill execution.
///
/// `entries` is the canonical mixed chain for both phases and steps. The
/// serialized `proofs` field remains only so stale data can be detected and
/// rejected by verification instead of being silently treated as authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofChain {
    /// Skill this chain is for
    pub skill: String,

    /// Session ID
    pub session_id: String,

    /// Genesis hash (always `GENESIS_HASH`)
    pub genesis_hash: String,

    /// Non-authoritative stale phase-only list. Runtime sealing writes phase
    /// proofs to `entries`; verification rejects chains where this field is
    /// non-empty.
    pub proofs: Vec<PhaseProof>,

    /// Mixed-entry ordered list (phases AND steps).
    #[serde(default)]
    pub entries: Vec<ProofEntry>,

    /// Whether all phases have been proven
    pub complete: bool,

    /// Whether the full chain has been verified
    pub chain_valid: bool,
}

/// Result of verifying Ed25519 signatures across a chain's proof entries.
#[derive(Debug, Clone)]
pub struct SignatureReport {
    /// Signed proof entries that verified against the key.
    pub verified: usize,
    /// Proof entries with no signature.
    pub unsigned: usize,
    /// Entry ids (`<phase_id>` or `<phase_id>.<step_id>`) that were unsigned or invalid.
    /// Non-empty ⇒ the chain fails.
    pub failures: Vec<String>,
}

impl SignatureReport {
    /// Whether every signable proof entry was signed and verified.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

impl ProofChain {
    /// Create a new empty chain for a skill
    #[must_use]
    pub fn new(skill: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            skill: skill.into(),
            session_id: session_id.into(),
            genesis_hash: GENESIS_HASH.to_string(),
            proofs: Vec::new(),
            entries: Vec::new(),
            complete: false,
            chain_valid: true,
        }
    }

    /// Get the hash that the next proof must reference as `previous_hash`.
    ///
    /// Resolution order: tail of `entries` (mixed chain), else `GENESIS_HASH`.
    #[must_use]
    pub fn head_hash(&self) -> &str {
        if let Some(entry) = self.entries.last() {
            return entry.combined_hash();
        }
        GENESIS_HASH
    }

    /// Canonical phase proofs in chain order.
    pub fn phase_entries(&self) -> impl Iterator<Item = &PhaseProof> {
        self.entries.iter().filter_map(|entry| match entry {
            ProofEntry::Phase(proof) => Some(proof),
            ProofEntry::Step(_) | ProofEntry::Disagreement(_) => None,
        })
    }

    /// Canonical phase-proof count.
    #[must_use]
    pub fn phase_count(&self) -> usize {
        self.phase_entries().count()
    }

    /// Canonical step-proof count.
    #[must_use]
    pub fn step_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, ProofEntry::Step(_)))
            .count()
    }

    /// Total canonical chain entries.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Whether every canonical phase proof has a sufficient judge verdict.
    #[must_use]
    pub fn phases_all_sufficient(&self) -> bool {
        self.phase_entries()
            .all(|proof| proof.judge_verdict.sufficient)
    }

    /// Verify Ed25519 signatures on every signable proof entry against `key`,
    /// failing closed.
    ///
    /// - signed + valid    → counted in `verified`
    /// - signed + invalid  → recorded as a failure (the proof was tampered with
    ///   after signing — e.g. a flipped judge verdict changes `combined_hash`,
    ///   so the signature over it no longer matches)
    /// - unsigned          → counted in `unsigned` and recorded as a failure
    ///
    /// This is the check that `verify()` deliberately omits because it has no
    /// verifying key: without it, an entry whose signed `combined_hash` was
    /// altered still passes hash-chain verification.
    #[must_use]
    pub fn verify_signatures(&self, key: &VerifyingKey) -> SignatureReport {
        let mut report = SignatureReport {
            verified: 0,
            unsigned: 0,
            failures: Vec::new(),
        };
        for entry in &self.entries {
            let id = entry.id();
            let result = match entry {
                ProofEntry::Phase(phase) => phase.verify_signature(key),
                ProofEntry::Step(step) => step.verify_signature(key),
                ProofEntry::Disagreement(marker) => marker.verify_signature(key),
            };
            match result {
                Ok(true) => report.verified += 1,
                Ok(false) => report.failures.push(id),
                Err(SignatureError::Missing) => {
                    report.unsigned += 1;
                    report.failures.push(id);
                }
                Err(_) => report.failures.push(id),
            }
        }
        report
    }

    /// **Attack #175 fix**: Maximum proofs per chain.
    /// Prevents disk/memory exhaustion from unbounded proof accumulation.
    /// 500 phases per skill is far beyond any legitimate workflow.
    const MAX_PROOFS_PER_CHAIN: usize = 500;

    /// Add a phase proof to the canonical mixed-entry chain.
    pub fn add_proof(&mut self, proof: PhaseProof) -> Result<(), ProofChainError> {
        self.add_phase_entry(proof)
    }

    /// Add a phase proof to the canonical mixed-entry chain.
    pub fn add_phase_entry(&mut self, proof: PhaseProof) -> Result<(), ProofChainError> {
        // **Attack #175 fix**: Reject proofs beyond the per-chain cap.
        if self.entries.len() + self.proofs.len() >= Self::MAX_PROOFS_PER_CHAIN {
            return Err(ProofChainError::ChainFull {
                skill: self.skill.clone(),
                max: Self::MAX_PROOFS_PER_CHAIN,
            });
        }

        // Verify the chain link
        if proof.previous_hash != self.head_hash() {
            return Err(ProofChainError::BrokenChain {
                phase: proof.phase_id.clone(),
                expected: self.head_hash().to_string(),
                got: proof.previous_hash,
            });
        }

        // Verify internal consistency
        if !proof.verify_self() {
            return Err(ProofChainError::InvalidProof {
                phase: proof.phase_id,
            });
        }

        self.entries.push(ProofEntry::Phase(proof));
        Ok(())
    }

    /// Add a step proof to the mixed-entry chain.
    ///
    /// Validates `previous_hash` matches the current head (which may itself
    /// be a phase or step entry — `head_hash()` resolves uniformly), runs
    /// the step's internal consistency check, and rejects if the chain has
    /// grown past the per-chain cap. Step entries count toward the same cap
    /// as phase entries to keep the disk-exhaustion guarantee from
    /// Attack #175 intact.
    pub fn add_step_proof(&mut self, proof: StepProof) -> Result<(), ProofChainError> {
        if self.entries.len() + self.proofs.len() >= Self::MAX_PROOFS_PER_CHAIN {
            return Err(ProofChainError::ChainFull {
                skill: self.skill.clone(),
                max: Self::MAX_PROOFS_PER_CHAIN,
            });
        }

        if proof.previous_hash != self.head_hash() {
            return Err(ProofChainError::BrokenChain {
                phase: format!("{}.{}", proof.phase_id, proof.step_id),
                expected: self.head_hash().to_string(),
                got: proof.previous_hash,
            });
        }

        if !proof.verify_self() {
            return Err(ProofChainError::InvalidProof {
                phase: format!("{}.{}", proof.phase_id, proof.step_id),
            });
        }

        self.entries.push(ProofEntry::Step(proof));
        Ok(())
    }

    /// Append a `DisagreementMarker` to the chain (#82 Stage B).
    /// Behaves like `add_step_proof` — capacity check, link check,
    /// internal-hash check — but for the multi-judge disagreement
    /// entry. Producers call this right after a `StepProof` when the
    /// per-step `MultiJudgeVerdict.disagreement` is `true`.
    ///
    /// # Errors
    ///
    /// Returns `ChainFull`, `BrokenChain`, or `InvalidProof` for
    /// the same reasons as the other `add_*` methods. The
    /// `BrokenChain.phase` field carries `disagreement(<phase>.<step>)`
    /// so error messages identify the marker source.
    pub fn add_disagreement(
        &mut self,
        marker: crate::disagreement::DisagreementMarker,
    ) -> Result<(), ProofChainError> {
        if self.entries.len() + self.proofs.len() >= Self::MAX_PROOFS_PER_CHAIN {
            return Err(ProofChainError::ChainFull {
                skill: self.skill.clone(),
                max: Self::MAX_PROOFS_PER_CHAIN,
            });
        }
        if marker.previous_hash != self.head_hash() {
            return Err(ProofChainError::BrokenChain {
                phase: format!("disagreement({}.{})", marker.phase_id, marker.step_id),
                expected: self.head_hash().to_string(),
                got: marker.previous_hash,
            });
        }
        if !marker.verify_self() {
            return Err(ProofChainError::InvalidProof {
                phase: format!("disagreement({}.{})", marker.phase_id, marker.step_id),
            });
        }
        self.entries.push(ProofEntry::Disagreement(marker));
        Ok(())
    }

    /// Verify the entire chain from genesis
    #[must_use]
    pub fn verify(&self) -> ChainVerification {
        let mut expected_previous = GENESIS_HASH.to_string();
        let mut errors = Vec::new();
        let mut last_completed: Option<DateTime<Utc>> = None;

        if !self.proofs.is_empty() {
            errors.push(format!(
                "unsupported phase-only proof vector contains {} entries; canonical proof chains must use entries",
                self.proofs.len()
            ));
        }

        for (i, entry) in self.entries.iter().enumerate() {
            if entry.previous_hash() != expected_previous {
                errors.push(format!(
                    "Entry {} ({}): expected previous_hash '{}', got '{}'",
                    i,
                    entry.id(),
                    expected_previous,
                    entry.previous_hash()
                ));
            }

            if !entry.verify_self() {
                errors.push(format!(
                    "Entry {} ({}): internal hash verification failed",
                    i,
                    entry.id()
                ));
            }

            if let Some(prev_completed) = last_completed {
                if entry.started_at() < prev_completed {
                    errors.push(format!(
                        "Entry {} ({}): started_at ({}) is before previous entry completed_at ({})",
                        i,
                        entry.id(),
                        entry.started_at(),
                        prev_completed
                    ));
                }
            }
            last_completed = Some(entry.completed_at());

            expected_previous = entry.combined_hash().to_string();
        }

        ChainVerification {
            valid: errors.is_empty(),
            phases_verified: self.phase_count(),
            steps_verified: self.step_count(),
            errors,
        }
    }
}

/// Result of verifying a proof chain
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerification {
    pub valid: bool,
    /// Phase entries seen in the canonical mixed chain.
    pub phases_verified: usize,
    /// Step entries seen in the canonical mixed chain.
    #[serde(default)]
    pub steps_verified: usize,
    pub errors: Vec<String>,
}

/// Errors in proof chain operations
#[derive(Debug)]
pub enum ProofChainError {
    BrokenChain {
        phase: String,
        expected: String,
        got: String,
    },
    InvalidProof {
        phase: String,
    },
    /// **Attack #175 fix**: Proof chain capacity exceeded.
    ChainFull {
        skill: String,
        max: usize,
    },
}

impl std::fmt::Display for ProofChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrokenChain {
                phase,
                expected,
                got,
            } => write!(
                f,
                "broken chain at phase '{phase}': expected previous_hash '{expected}', got '{got}'"
            ),
            Self::InvalidProof { phase } => {
                write!(
                    f,
                    "invalid proof for phase '{phase}': hash verification failed"
                )
            }
            Self::ChainFull { skill, max } => {
                write!(
                    f,
                    "proof chain for skill '{skill}' is full ({max} proofs max)"
                )
            }
        }
    }
}

impl std::error::Error for ProofChainError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::Evidence;
    use crate::judge::JudgeVerdict;

    fn make_proof(phase_id: &str, skill: &str, previous_hash: &str) -> PhaseProof {
        let evidence = Evidence::default();
        let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
        let combined_hash =
            PhaseProof::compute_combined_hash(phase_id, skill, &evidence_hash, previous_hash, true);

        PhaseProof {
            phase_id: phase_id.to_string(),
            skill: skill.to_string(),
            session_id: "test-session".to_string(),
            evidence,
            evidence_hash,
            previous_hash: previous_hash.to_string(),
            combined_hash,
            judge_model: "sonnet-4.6".to_string(),
            judge_verdict: JudgeVerdict {
                sufficient: true,
                confidence: 0.95,
                reasoning: "Evidence verified".to_string(),
                requested_evidence: None,
            },
            signature: None,
            started_at: Utc::now(),
            completed_at: Utc::now(),
            duration_ms: 100,
        }
    }

    #[test]
    fn test_genesis_chain() {
        let chain = ProofChain::new("linear", "sess-1");
        assert_eq!(chain.head_hash(), GENESIS_HASH);
        assert!(chain.proofs.is_empty());
    }

    #[test]
    fn test_add_valid_proof() {
        let mut chain = ProofChain::new("linear", "sess-1");
        let proof = make_proof("claim", "linear", GENESIS_HASH);
        assert!(chain.add_proof(proof).is_ok());
        assert_eq!(chain.phase_count(), 1);
        assert!(chain.proofs.is_empty(), "stale phase vector stays empty");
    }

    #[test]
    fn test_chain_links() {
        let mut chain = ProofChain::new("linear", "sess-1");

        let proof1 = make_proof("claim", "linear", GENESIS_HASH);
        chain.add_proof(proof1).unwrap();

        let proof2 = make_proof("fetch", "linear", chain.head_hash());
        chain.add_proof(proof2).unwrap();

        let proof3 = make_proof("intelligence", "linear", chain.head_hash());
        chain.add_proof(proof3).unwrap();

        assert_eq!(chain.phase_count(), 3);
        assert!(chain.proofs.is_empty(), "stale phase vector stays empty");
        let verification = chain.verify();
        assert!(verification.valid);
        assert_eq!(verification.phases_verified, 3);
    }

    #[test]
    fn test_broken_chain_detected() {
        let mut chain = ProofChain::new("linear", "sess-1");

        let proof1 = make_proof("claim", "linear", GENESIS_HASH);
        chain.add_proof(proof1).unwrap();

        // Try to add proof with wrong previous hash
        let bad_proof = make_proof("fetch", "linear", "wrong_hash");
        assert!(matches!(
            chain.add_proof(bad_proof),
            Err(ProofChainError::BrokenChain { .. })
        ));
    }

    #[test]
    fn test_tampered_proof_detected() {
        let mut chain = ProofChain::new("linear", "sess-1");

        let mut proof = make_proof("claim", "linear", GENESIS_HASH);
        // Tamper with evidence after hashing
        proof.evidence.phase_file_read = true; // Changed after hash was computed
        assert!(matches!(
            chain.add_proof(proof),
            Err(ProofChainError::InvalidProof { .. })
        ));
    }

    #[test]
    fn test_verify_full_chain() {
        let mut chain = ProofChain::new("linear", "sess-1");

        for phase in &["claim", "fetch", "intelligence", "plan-doc"] {
            let proof = make_proof(phase, "linear", chain.head_hash());
            chain.add_proof(proof).unwrap();
        }

        let result = chain.verify();
        assert!(result.valid);
        assert_eq!(result.phases_verified, 4);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_proof_hashes_are_deterministic() {
        let evidence = Evidence::default();
        let h1 = PhaseProof::compute_evidence_hash(&evidence);
        let h2 = PhaseProof::compute_evidence_hash(&evidence);
        assert_eq!(h1, h2);

        let c1 = PhaseProof::compute_combined_hash("claim", "linear", &h1, GENESIS_HASH, true);
        let c2 = PhaseProof::compute_combined_hash("claim", "linear", &h2, GENESIS_HASH, true);
        assert_eq!(c1, c2);
    }

    #[test]
    fn phase_combined_hash_golden_locks_the_preimage() {
        // Golden values pin the EXACT hash output for fixed inputs, so a silent
        // change to the preimage (field order, separators, the verdict byte)
        // is caught here even though every compute-then-verify test would still
        // pass (they recompute from the same formula). On-disk chains depend on
        // this preimage being stable; regenerate ONLY with a deliberate
        // migration. Preimage: phase_id || skill || evidence_hash ||
        // previous_hash || b"judge" || [u8::from(sufficient)].
        //
        // NOTE: these values changed when `skill` was bound into the preimage
        // (Phase 1) — a deliberate migration; pre-existing PhaseProofs re-seal.
        assert_eq!(
            PhaseProof::compute_combined_hash("claim", "linear", "ev", GENESIS_HASH, true),
            "9ab4a97f58feffb685707d70ecf9245fcf4a0546034a327073499a7e5f6bcaff",
        );
        // The verdict byte must change the digest (locks the security binding).
        assert_eq!(
            PhaseProof::compute_combined_hash("claim", "linear", "ev", GENESIS_HASH, false),
            "d28106912e86aa43c84a63cd9786749a35fa92dfe5a47af276f5c90c81990ad3",
        );
    }

    #[test]
    fn chain_signature_verification_fails_closed_on_tamper_and_forgery() {
        use ed25519_dalek::SigningKey;
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let vk = key.verifying_key();
        let art = || serde_json::json!({"k": "v"});

        // Correctly signed phase + step + disagreement marker → passes,
        // counted as verified.
        let mut good = ProofChain::new("linear", "sess");
        let mut phase = make_proof("claim", "linear", GENESIS_HASH);
        phase.sign_with(&key);
        good.add_proof(phase).expect("add signed phase");
        let mut step = make_step("s1", "claim", "linear", good.head_hash(), art());
        step.sign_with(&key);
        good.add_step_proof(step).expect("add signed step");
        let mut marker = make_disagreement("linear", "sess", "s1", "claim", good.head_hash());
        marker.sign_with(&key);
        good.add_disagreement(marker).expect("add signed marker");
        let rep = good.verify_signatures(&vk);
        assert!(rep.is_ok());
        assert_eq!(rep.verified, 3);

        // Wrong key must fail — the verifier can't be fooled by an arbitrary key.
        let wrong = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        assert!(!good.verify_signatures(&wrong).is_ok());

        // Forged/garbled signature with a still-valid combined_hash → fails
        // closed. (verify_self, and thus add_step_proof, accept it because the
        // hash is intact — this is exactly the gap signature verification closes.)
        let mut forged_chain = ProofChain::new("linear", "sess");
        let mut forged = make_step("s1", "claim", "linear", GENESIS_HASH, art());
        forged.sign_with(&key);
        forged.signature = Some("00".repeat(64));
        forged_chain
            .add_step_proof(forged)
            .expect("add (hash still valid)");
        assert!(
            !forged_chain.verify_signatures(&vk).is_ok(),
            "a forged signature must fail chain signature verification"
        );

        // Re-sealed verdict flip: flip the verdict AND recompute combined_hash so
        // verify_self passes — the signature (over the original hash) no longer
        // matches and can't be reforged without the private key. Caught here even
        // though hash-only verify() would pass it.
        let mut resealed_chain = ProofChain::new("linear", "sess");
        let mut t = make_step("s1", "claim", "linear", GENESIS_HASH, art());
        t.sign_with(&key);
        t.judge_verdict.sufficient = !t.judge_verdict.sufficient;
        t.combined_hash = StepProof::compute_combined_hash(
            &t.step_id,
            &t.phase_id,
            &t.skill,
            &t.evidence_hash,
            &t.artifact_hash,
            &t.previous_hash,
            t.judge_verdict.sufficient,
        );
        assert!(t.verify_self(), "re-sealed proof recomputes consistently");
        resealed_chain.add_step_proof(t).expect("add re-sealed");
        assert!(
            !resealed_chain.verify_signatures(&vk).is_ok(),
            "a re-sealed verdict flip must invalidate the signature"
        );

        // Unsigned step entry: fail closed.
        let mut unsigned_chain = ProofChain::new("linear", "sess");
        let unsigned = make_step("s1", "claim", "linear", GENESIS_HASH, art());
        unsigned_chain
            .add_step_proof(unsigned)
            .expect("add unsigned");
        let unsigned_report = unsigned_chain.verify_signatures(&vk);
        assert!(!unsigned_report.is_ok(), "unsigned must fail verification");
        assert_eq!(unsigned_report.unsigned, 1);
        assert_eq!(unsigned_report.failures, vec!["claim.s1"]);

        // Unsigned phase entry: fail closed too.
        let mut unsigned_phase_chain = ProofChain::new("linear", "sess");
        unsigned_phase_chain
            .add_proof(make_proof("claim", "linear", GENESIS_HASH))
            .expect("add unsigned phase");
        let unsigned_phase_report = unsigned_phase_chain.verify_signatures(&vk);
        assert!(
            !unsigned_phase_report.is_ok(),
            "unsigned phase must fail verification"
        );
        assert_eq!(unsigned_phase_report.unsigned, 1);
        assert_eq!(unsigned_phase_report.failures, vec!["claim"]);

        // Unsigned disagreement marker: fail closed.
        let mut unsigned_marker_chain = ProofChain::new("linear", "sess");
        let mut signed_step = make_step("s1", "claim", "linear", GENESIS_HASH, art());
        signed_step.sign_with(&key);
        unsigned_marker_chain
            .add_step_proof(signed_step)
            .expect("add signed step before marker");
        let marker_previous_hash = unsigned_marker_chain.head_hash().to_string();
        unsigned_marker_chain
            .add_disagreement(make_disagreement(
                "linear",
                "sess",
                "s1",
                "claim",
                &marker_previous_hash,
            ))
            .expect("add unsigned marker");
        let unsigned_marker_report = unsigned_marker_chain.verify_signatures(&vk);
        assert!(
            !unsigned_marker_report.is_ok(),
            "unsigned disagreement marker must fail verification"
        );
        assert_eq!(unsigned_marker_report.unsigned, 1);
        assert_eq!(
            unsigned_marker_report.failures,
            vec!["disagreement(claim.s1)"]
        );
    }

    #[test]
    fn flipping_judge_verdict_on_sealed_proof_breaks_verify_self() {
        // Security regression: the judge's gate outcome is folded into
        // combined_hash, so flipping `sufficient` on an already-sealed proof
        // (without re-sealing) must fail recomputation. Previously the verdict
        // was a free field — an attacker could turn a fail into a pass and the
        // proof (and its Ed25519 signature over combined_hash) still verified.
        let mut proof = make_proof("claim", "linear", GENESIS_HASH);
        assert!(proof.verify_self(), "freshly sealed proof must verify");

        proof.judge_verdict.sufficient = !proof.judge_verdict.sufficient;
        assert!(
            !proof.verify_self(),
            "a flipped judge verdict on a sealed proof must break verify_self"
        );
    }

    #[test]
    fn test_proof_rejects_backwards_timestamps() {
        let evidence = Evidence::default();
        let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
        let combined_hash = PhaseProof::compute_combined_hash(
            "claim",
            "linear",
            &evidence_hash,
            GENESIS_HASH,
            true,
        );

        let now = Utc::now();
        let proof = PhaseProof {
            phase_id: "claim".to_string(),
            skill: "linear".to_string(),
            session_id: "test-session".to_string(),
            evidence,
            evidence_hash,
            previous_hash: GENESIS_HASH.to_string(),
            combined_hash,
            judge_model: "sonnet-4.6".to_string(),
            judge_verdict: JudgeVerdict {
                sufficient: true,
                confidence: 0.95,
                reasoning: "Evidence verified".to_string(),
                requested_evidence: None,
            },
            signature: None,
            // completed_at BEFORE started_at — should fail verification
            started_at: now,
            completed_at: now - chrono::Duration::seconds(60),
            duration_ms: 100,
        };

        assert!(
            !proof.verify_self(),
            "Proof with completed_at < started_at should fail"
        );

        let mut chain = ProofChain::new("linear", "sess-1");
        assert!(
            chain.add_proof(proof).is_err(),
            "Chain should reject backwards-timestamp proof"
        );
    }

    #[test]
    fn test_different_evidence_different_hash() {
        let e1 = Evidence::default();
        let mut e2 = Evidence::default();
        e2.phase_file_read = true;

        let h1 = PhaseProof::compute_evidence_hash(&e1);
        let h2 = PhaseProof::compute_evidence_hash(&e2);
        assert_ne!(h1, h2);
    }

    // ── Mixed phase + step chain tests (M1.2) ────────────────────────────

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
            true,
        );
        StepProof {
            step_id: step_id.to_string(),
            phase_id: phase_id.to_string(),
            skill: skill.to_string(),
            session_id: "test-session".to_string(),
            evidence,
            evidence_hash,
            artifact,
            artifact_hash,
            account_context: None,
            previous_hash: previous_hash.to_string(),
            combined_hash,
            trace_context: None,
            judge_model: "sonnet-4.6".to_string(),
            judge_verdict: JudgeVerdict {
                sufficient: true,
                confidence: 0.95,
                reasoning: "ok".to_string(),
                requested_evidence: None,
            },
            signature: None,
            started_at: Utc::now(),
            completed_at: Utc::now(),
            duration_ms: 5,
        }
    }

    #[test]
    fn test_add_step_proof_to_empty_chain() {
        let mut chain = ProofChain::new("linear", "sess-1");
        let step = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        chain.add_step_proof(step).expect("add step on empty chain");
        assert_eq!(chain.entries.len(), 1);
        assert!(chain.proofs.is_empty(), "stale phase vector stays empty");
    }

    #[test]
    fn test_step_after_phase_chains_correctly() {
        // A common shape: skill starts with phase-level claim, then drops
        // into step-level execution. The step's previous_hash must point at
        // the phase's combined_hash.
        let mut chain = ProofChain::new("linear", "sess-1");
        let phase = make_proof("claim", "linear", GENESIS_HASH);
        chain.add_proof(phase).unwrap();

        let step = make_step(
            "1",
            "claim",
            "linear",
            chain.head_hash(),
            serde_json::json!({"ticket": "FPCRM-1"}),
        );
        chain.add_step_proof(step).expect("step after phase");

        let v = chain.verify();
        assert!(v.valid, "mixed chain must verify, errors: {:?}", v.errors);
        assert_eq!(v.phases_verified, 1);
        assert_eq!(v.steps_verified, 1);
    }

    #[test]
    fn test_multi_step_chain_with_typed_handoffs() {
        // Walk three steps through the chain, each consuming the prior
        // step's artifact hash as part of the chain link.
        let mut chain = ProofChain::new("linear", "sess-1");
        let s1 = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::json!({"ticket": "FPCRM-1"}),
        );
        chain.add_step_proof(s1).unwrap();

        let s2 = make_step(
            "2",
            "claim",
            "linear",
            chain.head_hash(),
            serde_json::json!({"branch": "fpcrm-1-fix"}),
        );
        chain.add_step_proof(s2).unwrap();

        let s3 = make_step(
            "1",
            "review",
            "linear",
            chain.head_hash(),
            serde_json::json!({"pr_url": "https://github.com/foo/bar/pull/9"}),
        );
        chain.add_step_proof(s3).unwrap();

        let v = chain.verify();
        assert!(
            v.valid,
            "multi-step chain must verify, errors: {:?}",
            v.errors
        );
        assert_eq!(v.steps_verified, 3);
        assert_eq!(v.phases_verified, 0, "no phase entries in this chain");
    }

    #[test]
    fn test_step_with_wrong_previous_hash_rejected() {
        let mut chain = ProofChain::new("linear", "sess-1");
        let step = make_step(
            "1",
            "claim",
            "linear",
            "deadbeef".repeat(8).as_str(),
            serde_json::Value::Null,
        );
        match chain.add_step_proof(step) {
            Err(ProofChainError::BrokenChain { .. }) => {}
            other => panic!("expected BrokenChain, got {other:?}"),
        }
    }

    #[test]
    fn test_step_with_tampered_artifact_rejected() {
        let mut chain = ProofChain::new("linear", "sess-1");
        let mut step = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::json!({"original": true}),
        );
        // Tamper after seal — combined hash and artifact hash still
        // reference the original payload, so verify_self() inside
        // add_step_proof must reject.
        step.artifact = serde_json::json!({"tampered": true});
        match chain.add_step_proof(step) {
            Err(ProofChainError::InvalidProof { .. }) => {}
            other => panic!("expected InvalidProof, got {other:?}"),
        }
    }

    #[test]
    fn test_phase_only_proof_vector_is_rejected() {
        let stale_json = format!(
            r#"{{
            "skill": "linear",
            "session_id": "sess-1",
            "genesis_hash": "0000000000000000000000000000000000000000000000000000000000000000",
            "proofs": [{}],
            "complete": false,
            "chain_valid": true
        }}"#,
            serde_json::to_string(&make_proof("claim", "linear", GENESIS_HASH)).unwrap()
        );
        let chain: ProofChain = serde_json::from_str(&stale_json).expect("stale chain loads");
        let verification = chain.verify();
        assert!(!verification.valid);
        assert!(
            verification
                .errors
                .iter()
                .any(|error| error.contains("unsupported phase-only proof vector")),
            "stale phase vector must fail verification: {:?}",
            verification.errors
        );
    }

    #[test]
    fn test_proof_entry_kind_tag_round_trips() {
        // ProofEntry must serialize with a `kind` discriminator so on-disk
        // chains stay self-describing — anyone reading the JSON can tell
        // a step entry from a phase entry without schema inference.
        let phase_entry = ProofEntry::Phase(make_proof("claim", "linear", GENESIS_HASH));
        let step_entry = ProofEntry::Step(make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        ));

        let phase_json = serde_json::to_string(&phase_entry).unwrap();
        let step_json = serde_json::to_string(&step_entry).unwrap();
        assert!(
            phase_json.contains(r#""kind":"phase""#),
            "phase tag missing: {phase_json}"
        );
        assert!(
            step_json.contains(r#""kind":"step""#),
            "step tag missing: {step_json}"
        );

        // Round trip back into Rust.
        let phase_back: ProofEntry = serde_json::from_str(&phase_json).unwrap();
        let step_back: ProofEntry = serde_json::from_str(&step_json).unwrap();
        assert!(matches!(phase_back, ProofEntry::Phase(_)));
        assert!(matches!(step_back, ProofEntry::Step(_)));
    }

    /// Helper: build a sample disagreement marker for a given chain head.
    fn make_disagreement(
        skill: &str,
        session_id: &str,
        step_id: &str,
        phase_id: &str,
        previous_hash: &str,
    ) -> crate::disagreement::DisagreementMarker {
        use crate::judge::{JudgeModel, JudgeVerdict};
        use crate::multi_judge::{JudgeRun, JudgeTrustTier, MultiJudgeVerdict};
        let mj = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                JudgeRun {
                    model: JudgeModel::Kimi,
                    verdict: JudgeVerdict::pass(0.95, "ok"),
                    cost_usd: None,
                    provider: None,
                },
                JudgeRun {
                    model: JudgeModel::Sonnet,
                    verdict: JudgeVerdict::fail(0.40, "no", vec![]),
                    cost_usd: None,
                    provider: None,
                },
            ],
        );
        crate::disagreement::DisagreementMarker::new(
            skill,
            session_id,
            step_id,
            phase_id,
            mj,
            previous_hash,
        )
    }

    #[test]
    fn add_disagreement_after_step_advances_chain() {
        // Real chain shape: StepProof → DisagreementMarker → next entry.
        // The marker's previous_hash must point at the step's combined_hash;
        // verify() must accept the resulting chain.
        let mut chain = ProofChain::new("linear", "sess-1");
        let step = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        chain.add_step_proof(step).unwrap();
        let marker = make_disagreement("linear", "sess-1", "1", "claim", chain.head_hash());
        chain
            .add_disagreement(marker)
            .expect("disagreement after step");
        assert_eq!(chain.entries.len(), 2);
        let v = chain.verify();
        assert!(
            v.valid,
            "chain with disagreement marker must verify, errors: {:?}",
            v.errors
        );
    }

    #[test]
    fn add_disagreement_with_wrong_previous_hash_rejected() {
        // The marker claims to follow GENESIS but the chain head is a
        // step. add_disagreement must reject with BrokenChain.
        let mut chain = ProofChain::new("linear", "sess-1");
        let step = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        chain.add_step_proof(step).unwrap();
        // Wrong previous_hash — points at GENESIS, not the step's hash.
        let bad = make_disagreement("linear", "sess-1", "1", "claim", GENESIS_HASH);
        assert!(matches!(
            chain.add_disagreement(bad),
            Err(ProofChainError::BrokenChain { .. })
        ));
    }

    #[test]
    fn add_tampered_disagreement_rejected() {
        // The marker passes the chain-link check but its multi_judge_hash
        // doesn't match the verdict bytes. add_disagreement must reject
        // with InvalidProof.
        let mut chain = ProofChain::new("linear", "sess-1");
        let step = make_step(
            "1",
            "claim",
            "linear",
            GENESIS_HASH,
            serde_json::Value::Null,
        );
        chain.add_step_proof(step).unwrap();
        let mut marker = make_disagreement("linear", "sess-1", "1", "claim", chain.head_hash());
        // Tamper after construction — combined_hash and multi_judge_hash
        // no longer match the captured verdict.
        marker.multi_judge.sufficient = !marker.multi_judge.sufficient;
        assert!(matches!(
            chain.add_disagreement(marker),
            Err(ProofChainError::InvalidProof { .. })
        ));
    }

    #[test]
    fn proof_entry_disagreement_serde_tagged_correctly() {
        // The chain JSON format MUST tag the new variant as
        // {"kind":"disagreement",...}. Round-trip pinned so a future
        // serde rename can't silently shift the wire format.
        let mj_marker = make_disagreement("linear", "sess-1", "1", "claim", GENESIS_HASH);
        let entry = ProofEntry::Disagreement(mj_marker);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains(r#""kind":"disagreement""#),
            "disagreement tag missing: {json}",
        );
        let back: ProofEntry = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ProofEntry::Disagreement(_)));
    }

    #[test]
    fn proof_entry_combined_hash_dispatches_to_disagreement_variant() {
        // ProofEntry::combined_hash() must return the marker's
        // combined_hash for the Disagreement variant, not panic or
        // return wrong data. Pins the match arm.
        let marker = make_disagreement("linear", "sess-1", "1", "claim", GENESIS_HASH);
        let expected = marker.combined_hash.clone();
        let entry = ProofEntry::Disagreement(marker);
        assert_eq!(entry.combined_hash(), expected);
    }
}
