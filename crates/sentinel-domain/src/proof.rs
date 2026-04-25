//! Proof-of-Work Chain
//!
//! Cryptographic proof chain for skill phase execution.
//! Each phase produces a hash from (`phase_id` + `evidence_hash` + `previous_hash`),
//! creating a tamper-evident chain — same trust model as blockchain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence::Evidence;
use crate::judge::JudgeVerdict;

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

    /// Compute the combined hash (the tessera)
    pub fn compute_combined_hash(
        phase_id: &str,
        evidence_hash: &str,
        previous_hash: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(phase_id.as_bytes());
        hasher.update(evidence_hash.as_bytes());
        hasher.update(previous_hash.as_bytes());
        format!("{:x}", hasher.finalize())
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

        // Verify combined hash
        let expected_combined =
            Self::compute_combined_hash(&self.phase_id, &self.evidence_hash, &self.previous_hash);
        self.combined_hash == expected_combined
    }
}

/// The full chain for a skill execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofChain {
    /// Skill this chain is for
    pub skill: String,

    /// Session ID
    pub session_id: String,

    /// Genesis hash (always `GENESIS_HASH`)
    pub genesis_hash: String,

    /// Ordered list of phase proofs
    pub proofs: Vec<PhaseProof>,

    /// Whether all phases have been proven
    pub complete: bool,

    /// Whether the full chain has been verified
    pub chain_valid: bool,
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
            complete: false,
            chain_valid: true,
        }
    }

    /// Get the hash that the next proof must reference as `previous_hash`
    #[must_use]
    pub fn head_hash(&self) -> &str {
        self.proofs
            .last()
            .map_or(GENESIS_HASH, |p| &p.combined_hash)
    }

    /// **Attack #175 fix**: Maximum proofs per chain.
    /// Prevents disk/memory exhaustion from unbounded proof accumulation.
    /// 500 phases per skill is far beyond any legitimate workflow.
    const MAX_PROOFS_PER_CHAIN: usize = 500;

    /// Add a proof to the chain. Returns error if `previous_hash` doesn't match.
    pub fn add_proof(&mut self, proof: PhaseProof) -> Result<(), ProofChainError> {
        // **Attack #175 fix**: Reject proofs beyond the per-chain cap.
        if self.proofs.len() >= Self::MAX_PROOFS_PER_CHAIN {
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

        self.proofs.push(proof);
        Ok(())
    }

    /// Verify the entire chain from genesis
    #[must_use]
    pub fn verify(&self) -> ChainVerification {
        let mut expected_previous = GENESIS_HASH.to_string();
        let mut errors = Vec::new();
        let mut last_completed: Option<DateTime<Utc>> = None;

        for (i, proof) in self.proofs.iter().enumerate() {
            // Check chain link
            if proof.previous_hash != expected_previous {
                errors.push(format!(
                    "Phase {} ({}): expected previous_hash '{}', got '{}'",
                    i, proof.phase_id, expected_previous, proof.previous_hash
                ));
            }

            // Check internal consistency (includes timestamp ordering)
            if !proof.verify_self() {
                errors.push(format!(
                    "Phase {} ({}): internal hash verification failed",
                    i, proof.phase_id
                ));
            }

            // **Attack #170 fix**: Cross-phase temporal ordering.
            // Phase N must start at or after Phase N-1 completed.
            if let Some(prev_completed) = last_completed {
                if proof.started_at < prev_completed {
                    errors.push(format!(
                        "Phase {} ({}): started_at ({}) is before previous phase completed_at ({})",
                        i, proof.phase_id, proof.started_at, prev_completed
                    ));
                }
            }
            last_completed = Some(proof.completed_at);

            expected_previous = proof.combined_hash.clone();
        }

        ChainVerification {
            valid: errors.is_empty(),
            phases_verified: self.proofs.len(),
            errors,
        }
    }
}

/// Result of verifying a proof chain
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerification {
    pub valid: bool,
    pub phases_verified: usize,
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
                write!(f, "invalid proof for phase '{phase}': hash verification failed")
            }
            Self::ChainFull { skill, max } => {
                write!(f, "proof chain for skill '{skill}' is full ({max} proofs max)")
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
            PhaseProof::compute_combined_hash(phase_id, &evidence_hash, previous_hash);

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
        assert_eq!(chain.proofs.len(), 1);
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

        assert_eq!(chain.proofs.len(), 3);
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

        let c1 = PhaseProof::compute_combined_hash("claim", &h1, GENESIS_HASH);
        let c2 = PhaseProof::compute_combined_hash("claim", &h2, GENESIS_HASH);
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_proof_rejects_backwards_timestamps() {
        let evidence = Evidence::default();
        let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
        let combined_hash =
            PhaseProof::compute_combined_hash("claim", &evidence_hash, GENESIS_HASH);

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
}
