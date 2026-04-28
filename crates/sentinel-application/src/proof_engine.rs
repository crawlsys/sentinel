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

    /// Minimum seconds between resubmissions after a failure.
    const RESUBMIT_COOLDOWN_SECS: i64 = sentinel_domain::constants::PROOF_RESUBMIT_COOLDOWN_SECS;

    /// Maximum consecutive failures before requiring longer cooldown.
    const MAX_RAPID_FAILURES: u32 = sentinel_domain::constants::PROOF_MAX_RAPID_FAILURES;

    /// Submit evidence for a phase and build its proof.
    /// `workflow` is optional — if provided, enables sequential phase enforcement.
    pub async fn submit_evidence(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: Evidence,
        judge_model: JudgeModel,
        started_at: chrono::DateTime<Utc>,
        workflow: Option<&sentinel_domain::workflow::SkillWorkflow>,
    ) -> Result<PhaseProof> {
        // Check resubmission rate limit. Cooldown logic + state inspection
        // both live on `SessionState` — we just ask whether a wait is needed.
        let phase_key = format!("{skill}:{phase_id}");
        {
            let state = self.state.read().await;
            if let Some(remaining) = state.submission_cooldown_remaining(
                &phase_key,
                Self::MAX_RAPID_FAILURES,
                Self::RESUBMIT_COOLDOWN_SECS,
            ) {
                let count = state.submission_attempts(&phase_key).map_or(0, |a| a.count);
                bail!(
                    "Phase '{}' resubmission blocked — wait {}s (failed {} time(s))",
                    phase_id,
                    remaining,
                    count
                );
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
            self.state
                .write()
                .await
                .record_submission_failure(phase_key);
            bail!(
                "Phase '{}' evidence insufficient: {}",
                phase_id,
                verdict.reasoning
            );
        }

        // Clear failure tracking on success
        self.state
            .write()
            .await
            .clear_submission_failure(&phase_key);

        // Compute hashes, build proof, and add to chain under a single write lock
        // to prevent TOCTOU races on concurrent submissions
        let (proof, combined_hash) = {
            let mut state = self.state.write().await;
            let completed_at = Utc::now();

            let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
            let previous_hash = state.proof_chains.get(skill).map_or_else(
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
                duration_ms: (completed_at - started_at)
                    .num_milliseconds()
                    .unsigned_abs(),
            };

            // Add to chain
            let session_id = state.session_id.clone();
            let chain = state
                .proof_chains
                .entry(skill.to_string())
                .or_insert_with(|| ProofChain::new(skill, &session_id));
            chain.add_proof(proof.clone())?;

            // **Attack #62 fix**: Use advance_sequential() when workflow definition
            // is available. This prevents out-of-order phase completion even if the
            // AI judge approves evidence for a later phase.
            if let Some(wf) = state.workflows.get_mut(skill) {
                if wf.is_phase_complete(phase_id) {
                    // Already complete — idempotent, no-op
                } else if let Some(wf_def) = workflow {
                    // Sequential enforcement: phase must be the next required one
                    if wf.advance_sequential(phase_id, wf_def) {
                        eprintln!(
                            "[sentinel] ProofEngine: Sequentially advanced phase '{}' for skill '{}' \
                             (now {} completed phases)",
                            phase_id, skill, wf.completed_phases.len()
                        );
                    } else {
                        bail!(
                            "Phase '{}' cannot be advanced — prior required phases are incomplete. \
                             Phases must be completed in order.",
                            phase_id
                        );
                    }
                } else {
                    // **Attack #79 fix**: No workflow definition available — fail closed.
                    // Previously fell back to advance_unchecked() which bypasses sequential
                    // enforcement. Now refuse to advance without a workflow definition.
                    // ProofEngine callers MUST provide a workflow parameter.
                    bail!(
                        "Phase '{}' for skill '{}' cannot be advanced — no workflow definition provided. \
                         ProofEngine requires workflow context for sequential enforcement.",
                        phase_id, skill
                    );
                }
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
