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
use sentinel_domain::step_proof::StepProof;

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
                // Praefectus wiring deferred (Fabrica task #24 phase 2c).
                actor: None,
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

    /// Submit a verdict for a single step within a phase. Builds the
    /// [`StepProof`], appends it to the active chain via
    /// [`ProofChain::add_step_proof`], and returns the sealed proof.
    ///
    /// Differs from [`submit_evidence`](Self::submit_evidence) in three ways:
    ///
    /// 1. **Verdict is supplied, not produced.** Caller has already run the
    ///    judge (typically via [`step_judge`](crate::hooks::step_judge),
    ///    which returns `StepJudgeOutcome::Judged { verdict, .. }`). This
    ///    keeps the judge call out of the write lock and lets cross-vendor
    ///    parallel judging (#73) happen upstream without changing this
    ///    method's signature.
    ///
    /// 2. **No phase-advancement side effect.** Step granularity is finer
    ///    than phase boundaries; we don't touch `WorkflowState` phase
    ///    progress here. `step_gate` (M1.3) consumes the step proofs we
    ///    write to know when to allow the next tool call.
    ///
    /// 3. **Insufficient verdicts hard-fail with no chain mutation.** If
    ///    `verdict.sufficient == false`, we return an error without
    ///    appending — the chain only carries verdicts that passed. Failed
    ///    verdicts are still observable via the JudgeError surface in
    ///    `step_judge` and via tracing logs.
    ///
    /// On success, returns the sealed `StepProof` so callers can hash its
    /// `combined_hash` into downstream artifacts (e.g. virtual skill pack
    /// edge metadata in M7).
    pub async fn submit_step_evidence(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        step_description: &str,
        evidence: Evidence,
        verdict: JudgeVerdict,
        judge_model: JudgeModel,
        artifact: serde_json::Value,
        account_context: Option<String>,
        started_at: chrono::DateTime<Utc>,
    ) -> Result<StepProof> {
        info!(
            skill,
            phase = phase_id,
            step = step_id,
            sufficient = verdict.sufficient,
            confidence = verdict.confidence,
            "Step verdict received"
        );

        if !verdict.sufficient {
            bail!(
                "Step '{phase_id}.{step_id}' (skill '{skill}') evidence \
                 insufficient — refusing to seal StepProof. Reason: {reason}. \
                 Step description: '{step_description}'.",
                reason = verdict.reasoning,
            );
        }

        // Hash + chain mutation under a single write lock to prevent TOCTOU
        // races on concurrent submissions, mirroring submit_evidence.
        let proof = {
            let mut state = self.state.write().await;
            let completed_at = Utc::now();

            let evidence_hash = StepProof::compute_evidence_hash(&evidence);
            let artifact_hash = StepProof::compute_artifact_hash(&artifact);
            let previous_hash = state.proof_chains.get(skill).map_or_else(
                || sentinel_domain::proof::GENESIS_HASH.to_string(),
                |chain| chain.head_hash().to_string(),
            );
            let combined_hash = StepProof::compute_combined_hash(
                step_id,
                phase_id,
                skill,
                &evidence_hash,
                &artifact_hash,
                &previous_hash,
            );

            let proof = StepProof {
                step_id: step_id.to_string(),
                phase_id: phase_id.to_string(),
                skill: skill.to_string(),
                session_id: state.session_id.clone(),
                evidence,
                evidence_hash,
                artifact,
                artifact_hash,
                account_context,
                previous_hash,
                combined_hash: combined_hash.clone(),
                judge_model: judge_model.to_string(),
                judge_verdict: verdict,
                signature: None, // M1.7 (Ed25519 opt-in) wires this when configured
                trace_context: None, // M4.5 — exporter wiring lands separately
                started_at,
                completed_at,
                duration_ms: (completed_at - started_at)
                    .num_milliseconds()
                    .unsigned_abs(),
            };

            // Append to chain — creates a fresh ProofChain on first step.
            let session_id = state.session_id.clone();
            let chain = state
                .proof_chains
                .entry(skill.to_string())
                .or_insert_with(|| ProofChain::new(skill, &session_id));
            chain.add_step_proof(proof.clone())?;

            debug!(
                skill,
                phase = phase_id,
                step = step_id,
                tessera = &combined_hash[..12],
                "StepProof added to chain"
            );

            proof
        };

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

#[cfg(test)]
mod step_evidence_tests {
    //! Tests for `submit_step_evidence` (M1.5). Verifies the step-level
    //! write side of the proof chain: passing verdicts seal a StepProof
    //! into the chain, failing verdicts hard-fail without mutation,
    //! sequential steps chain correctly via head_hash().

    use super::*;
    use crate::judge_service::JudgeService;
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
    use sentinel_domain::proof::ProofEntry;

    /// Test double: returns whatever verdict it was constructed with.
    /// submit_step_evidence doesn't actually call the judge — the judge
    /// runs in step_judge (M1.4) upstream — so this stub mostly exists
    /// to satisfy ProofEngine::new's signature.
    struct StubJudge;
    #[async_trait::async_trait]
    impl JudgeService for StubJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            unreachable!("submit_step_evidence does not call evaluate()")
        }
    }

    fn engine() -> ProofEngine {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        ProofEngine::new(state, Arc::new(StubJudge))
    }

    #[tokio::test]
    async fn passing_verdict_seals_step_proof_into_chain() {
        let eng = engine();
        let result = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "Open PR with Ref FPCRM-XXX",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::json!({"pr_url": "https://github.com/foo/bar/pull/9"}),
                Some("firefly-pro".into()),
                Utc::now() - chrono::Duration::milliseconds(50),
            )
            .await
            .expect("passing verdict seals proof");

        // StepProof should self-verify and be findable in the chain.
        assert!(result.verify_self());
        assert_eq!(result.skill, "linear");
        assert_eq!(result.step_id, "1");
        assert_eq!(result.account_context.as_deref(), Some("firefly-pro"));

        let state = eng.state.read().await;
        let chain = state.proof_chains.get("linear").expect("chain exists");
        assert_eq!(chain.entries.len(), 1, "exactly one step entry sealed");
        match &chain.entries[0] {
            ProofEntry::Step(s) => {
                assert_eq!(s.combined_hash, result.combined_hash);
                assert_eq!(s.previous_hash, sentinel_domain::proof::GENESIS_HASH);
            }
            _ => panic!("expected Step entry"),
        }
    }

    #[tokio::test]
    async fn insufficient_verdict_hard_fails_without_mutating_chain() {
        let eng = engine();
        let res = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "Open PR with Ref FPCRM-XXX",
                Evidence::default(),
                JudgeVerdict::fail(
                    0.7,
                    "PR body missing FPCRM ref",
                    vec!["Ref FPCRM-1 in PR body".into()],
                ),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await;

        assert!(res.is_err(), "insufficient verdict must error");
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("insufficient"),
            "error mentions 'insufficient', got: {err}"
        );
        assert!(
            err.contains("PR body missing"),
            "error includes judge reasoning"
        );

        // No chain mutation on failure.
        let state = eng.state.read().await;
        assert!(
            !state.proof_chains.contains_key("linear"),
            "no chain should be created when verdict fails",
        );
    }

    #[tokio::test]
    async fn sequential_step_proofs_chain_via_head_hash() {
        let eng = engine();

        // Each step's `started_at` must be >= the prior step's
        // `completed_at` (chain temporal ordering — Attack #170 parity).
        // Use Utc::now() right before each submit so the engine's
        // `completed_at = Utc::now()` inside the call lands AFTER our
        // started_at by a few microseconds.
        let p1 = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"ticket": "FPCRM-1"}),
                None,
                Utc::now(),
            )
            .await
            .expect("step 1");

        // Sleep so step 2's started_at is strictly after step 1's
        // completed_at. 50ms is generous to absorb scheduling jitter
        // when this test runs alongside the rest of the suite under
        // load (parallel test runner, slow CI runners, etc).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let p2 = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "2",
                "create branch",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"branch": "fpcrm-1-fix"}),
                None,
                Utc::now(),
            )
            .await
            .expect("step 2");

        // Step 2's previous_hash must equal step 1's combined_hash —
        // that's the chain link.
        assert_eq!(
            p2.previous_hash, p1.combined_hash,
            "step 2 must chain to step 1 via combined_hash",
        );

        // Full chain verifies cleanly.
        let verification = eng.verify_chain("linear").await.expect("chain verifies");
        assert!(verification.valid, "errors: {:?}", verification.errors);
        assert_eq!(verification.steps_verified, 2);
        assert_eq!(verification.phases_verified, 0);
    }

    #[tokio::test]
    async fn step_after_existing_phase_proof_chains_correctly() {
        // Realistic mixed chain: skill starts with a phase-level claim
        // proof (legacy `proofs` Vec), then drops into step-level work
        // (mixed `entries` Vec). The step's previous_hash must point at
        // the phase's combined_hash via head_hash().
        let eng = engine();

        // Pre-seed the chain with a PhaseProof to simulate prior phase
        // execution. We bypass submit_evidence here because that calls
        // the judge — for this test we only care about chain linkage.
        {
            let mut state = eng.state.write().await;
            let mut chain = ProofChain::new("linear", "test-session");
            let evidence = Evidence::default();
            let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
            let combined_hash = PhaseProof::compute_combined_hash(
                "claim",
                &evidence_hash,
                sentinel_domain::proof::GENESIS_HASH,
            );
            let phase_proof = PhaseProof {
                phase_id: "claim".into(),
                skill: "linear".into(),
                session_id: "test-session".into(),
                evidence,
                evidence_hash,
                previous_hash: sentinel_domain::proof::GENESIS_HASH.into(),
                combined_hash: combined_hash.clone(),
                judge_model: "sonnet".into(),
                judge_verdict: JudgeVerdict::pass(0.95, "claimed"),
                started_at: Utc::now() - chrono::Duration::seconds(10),
                completed_at: Utc::now() - chrono::Duration::seconds(5),
                duration_ms: 5000,
                actor: None,
            };
            chain.add_proof(phase_proof).expect("seed phase");
            state.proof_chains.insert("linear".into(), chain);
        }

        // Now submit a step. Its previous_hash should match the phase's
        // combined_hash because head_hash() prefers entries-tail then
        // proofs-tail, and there's no entries-tail yet.
        let phase_combined = {
            let state = eng.state.read().await;
            state
                .proof_chains
                .get("linear")
                .unwrap()
                .head_hash()
                .to_string()
        };

        let step = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "first step after phase",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect("step seals after phase");

        assert_eq!(
            step.previous_hash, phase_combined,
            "step's previous_hash must match the phase's combined_hash",
        );

        // The chain should now have 1 phase + 1 step and verify clean.
        let verification = eng.verify_chain("linear").await.expect("verifies");
        assert!(
            verification.valid,
            "mixed chain errors: {:?}",
            verification.errors
        );
        assert_eq!(verification.phases_verified, 1);
        assert_eq!(verification.steps_verified, 1);
    }

    #[tokio::test]
    async fn artifact_is_hashed_into_combined_hash() {
        // Two otherwise-identical step submissions with different
        // artifacts must produce different combined_hashes — that's the
        // typed-handoff tamper-evidence property from M1.1.
        let eng_a = engine();
        let eng_b = engine();
        let started = Utc::now();

        let a = eng_a
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "open PR",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"pr_url": "https://x/1"}),
                None,
                started,
            )
            .await
            .unwrap();

        let b = eng_b
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "open PR",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"pr_url": "https://x/2"}), // different artifact
                None,
                started,
            )
            .await
            .unwrap();

        assert_ne!(
            a.combined_hash, b.combined_hash,
            "different artifacts must produce different combined hashes",
        );
        assert_ne!(a.artifact_hash, b.artifact_hash);
    }
}
