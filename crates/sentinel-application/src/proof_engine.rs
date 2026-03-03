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
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
use sentinel_domain::proof::{PhaseProof, ProofChain};
use sentinel_domain::state::SessionState;

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
            bail!(
                "Phase '{}' evidence insufficient: {}",
                phase_id,
                verdict.reasoning
            );
        }

        // Compute hashes
        let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
        let previous_hash = {
            let state = self.state.read().await;
            state
                .proof_chains
                .get(skill)
                .map_or_else(
                    || sentinel_domain::proof::GENESIS_HASH.to_string(),
                    |chain| chain.head_hash().to_string(),
                )
        };
        let combined_hash =
            PhaseProof::compute_combined_hash(phase_id, &evidence_hash, &previous_hash);

        let proof = PhaseProof {
            phase_id: phase_id.to_string(),
            skill: skill.to_string(),
            session_id: self.state.read().await.session_id.clone(),
            evidence,
            evidence_hash,
            previous_hash,
            combined_hash: combined_hash.clone(),
            judge_model: judge_model.to_string(),
            judge_verdict: verdict,
            started_at,
            completed_at: Utc::now(),
            duration_ms: (Utc::now() - started_at).num_milliseconds().unsigned_abs(),
        };

        // Add to chain
        {
            let mut state = self.state.write().await;
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
        }

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
