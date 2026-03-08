//! Proof Engine
//!
//! Manages proof chains: creates proofs from evidence, adds them to chains,
//! verifies chain integrity. Coordinates with AI judges.

use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::JudgeModel;
use sentinel_domain::proof::{PhaseProof, ProofChain};
use sentinel_domain::state::{SessionState, SubmissionAttempts};

use crate::judge_service::JudgeService;

/// Proof engine — builds and verifies proof chains
pub struct ProofEngine {
    /// Shared session state
    state: Arc<RwLock<SessionState>>,

    /// AI judge service
    judge: Arc<dyn JudgeService>,
}

impl ProofEngine {
    pub fn new(state: Arc<RwLock<SessionState>>, judge: Arc<dyn JudgeService>) -> Self {
        Self { state, judge }
    }

    /// Minimum seconds between resubmissions after a failure
    const RESUBMIT_COOLDOWN_SECS: i64 = 30;

    /// Maximum consecutive failures before requiring longer cooldown
    const MAX_RAPID_FAILURES: u32 = 3;

    /// Submit evidence for a phase and build its proof
    pub async fn submit_evidence(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: Evidence,
        judge_model: JudgeModel,
        started_at: chrono::DateTime<Utc>,
    ) -> Result<PhaseProof> {
        // Check resubmission rate limit
        let phase_key = format!("{skill}:{phase_id}");
        {
            let state = self.state.read().await;
            if let Some(attempts) = state.failed_submissions.get(&phase_key) {
                if let Some(last) = attempts.last_failure {
                    let elapsed = (Utc::now() - last).num_seconds();
                    let cooldown = if attempts.count >= Self::MAX_RAPID_FAILURES {
                        Self::RESUBMIT_COOLDOWN_SECS * attempts.count as i64
                    } else {
                        Self::RESUBMIT_COOLDOWN_SECS
                    };
                    if elapsed < cooldown {
                        bail!(
                            "Phase '{}' resubmission blocked — wait {}s (failed {} time(s))",
                            phase_id,
                            cooldown - elapsed,
                            attempts.count
                        );
                    }
                }
            }
        }

        // Ask AI judge to evaluate the evidence
        let verdict = self
            .judge
            .evaluate(skill, phase_id, phase_objectives, &evidence, judge_model)
            .await?;

        info!(
            phase = phase_id,
            skill,
            sufficient = verdict.sufficient,
            confidence = verdict.confidence,
            "Judge verdict received"
        );

        if !verdict.sufficient {
            // Track the failure for rate limiting
            {
                let mut state = self.state.write().await;
                let attempts = state
                    .failed_submissions
                    .entry(phase_key)
                    .or_insert_with(SubmissionAttempts::default);
                attempts.count += 1;
                attempts.last_failure = Some(Utc::now());
            }
            bail!(
                "Phase '{}' evidence insufficient: {}",
                phase_id,
                verdict.reasoning
            );
        }

        // Clear failure tracking on success
        {
            let mut state = self.state.write().await;
            state.failed_submissions.remove(&phase_key);
        }

        // Compute hashes, build proof, and add to chain under a single write lock
        // to prevent TOCTOU races on concurrent submissions
        let (proof, combined_hash) = {
            let mut state = self.state.write().await;
            let completed_at = Utc::now();

            let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
            let previous_hash = state
                .proof_chains
                .get(skill)
                .map_or_else(
                    || sentinel_domain::proof::GENESIS_HASH.to_string(),
                    |chain| chain.head_hash().to_string(),
                );
            let combined_hash =
                PhaseProof::compute_combined_hash(phase_id, &evidence_hash, &previous_hash);

            let proof = PhaseProof {
                phase_id: phase_id.to_string(),
                skill: skill.to_string(),
                session_id: state.session_id.clone(),
                evidence,
                evidence_hash,
                previous_hash,
                combined_hash: combined_hash.clone(),
                judge_model: judge_model.to_string(),
                judge_verdict: verdict,
                started_at,
                completed_at,
                duration_ms: (completed_at - started_at).num_milliseconds().unsigned_abs(),
            };

            // Add to chain
            let session_id = state.session_id.clone();
            let chain = state
                .proof_chains
                .entry(skill.to_string())
                .or_insert_with(|| ProofChain::new(skill, &session_id));
            chain.add_proof(proof.clone())?;

            // Advance workflow
            if let Some(wf) = state.workflows.get_mut(skill) {
                wf.advance(phase_id);
            }

            (proof, combined_hash)
        };

        debug!(
            phase = phase_id,
            tessera = &combined_hash[..12],
            "Proof added to chain"
        );

        Ok(proof)
    }

    /// Verify a skill's proof chain
    pub async fn verify_chain(
        &self,
        skill: &str,
    ) -> Result<sentinel_domain::proof::ChainVerification> {
        let state = self.state.read().await;
        let chain = state
            .proof_chains
            .get(skill)
            .ok_or_else(|| anyhow::anyhow!("No proof chain for skill '{}'", skill))?;
        Ok(chain.verify())
    }
}
