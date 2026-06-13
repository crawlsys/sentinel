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
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::judge_service::JudgeService;

/// Proof engine — builds and verifies proof chains
pub struct ProofEngine {
    /// Shared session state
    state: Arc<RwLock<SessionState>>,

    /// AI judge service
    judge: Arc<dyn JudgeService>,

    /// Optional Ed25519 signing key (#4 — proof attestation). When present,
    /// every sealed `StepProof` is signed over its `combined_hash`, so verifiers
    /// can confirm "the holder of this key authored this chain entry" — beyond
    /// the SHA-256 hash chain. Loaded from `SENTINEL_SIGNING_KEY` by the CLI
    /// layer (sentinel-domain stays pure / key-agnostic). `None` = unsigned
    /// (hash-chain integrity only), the back-compat default.
    signing_key: Option<SigningKey>,

    /// When true, sealing REFUSES to proceed without a signing key — the
    /// mandatory-attestation posture for audit-grade deployments. Set from
    /// `SENTINEL_SIGNING_REQUIRED`. With no key configured, every seal errors
    /// rather than silently writing an unsigned (un-attestable) proof.
    signing_required: bool,

    /// Optional Ed25519 PUBLIC key for verifying signatures during chain
    /// verification. Loaded from `SENTINEL_VERIFY_KEY` by the CLI layer. When
    /// present, `verify_chain` checks every signed `StepProof` and fails closed
    /// on a present-but-invalid signature (and on unsigned entries when
    /// `signing_required`). Deliberately independent of `signing_key`: deriving
    /// the verify key from the signing key would let whoever holds the signing
    /// key (potentially the agent) re-sign a forged chain. `None` = signatures
    /// not checked (hash-chain integrity only).
    verify_key: Option<VerifyingKey>,
}

impl ProofEngine {
    pub fn new(state: Arc<RwLock<SessionState>>, judge: Arc<dyn JudgeService>) -> Self {
        Self {
            state,
            judge,
            signing_key: None,
            signing_required: false,
            verify_key: None,
        }
    }

    /// Wire an Ed25519 signing key + the mandatory-signing posture (#4).
    /// When `key` is `Some`, every sealed `StepProof` is signed. When
    /// `required` is true, sealing without a key is refused (error) — the
    /// audit-grade attestation guarantee. Builder shape; existing callers
    /// keep the unsigned default.
    #[must_use]
    pub fn with_signing(mut self, key: Option<SigningKey>, required: bool) -> Self {
        self.signing_key = key;
        self.signing_required = required;
        self
    }

    /// Wire the Ed25519 PUBLIC verifying key used by [`verify_chain`]. When
    /// `Some`, chain verification additionally checks every signed `StepProof`
    /// and fails closed on a bad signature (and on unsigned entries when the
    /// mandatory-signing posture is set). Loaded from `SENTINEL_VERIFY_KEY`.
    #[must_use]
    pub fn with_verify_key(mut self, key: Option<VerifyingKey>) -> Self {
        self.verify_key = key;
        self
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
        dual: bool,
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
                    "Phase '{phase_id}' resubmission blocked — wait {remaining}s (failed {count} time(s))"
                );
            }
        }

        // Ask AI judge to evaluate the evidence. For high-stakes (`dual`)
        // phases the verdict comes from the cross-vendor DualFrontier tier
        // (Opus 4.8 + GPT-5.5), folded conservatively into a single verdict;
        // otherwise the single configured `judge_model` runs.
        let verdict = self
            .judge_verdict_for(skill, phase_id, phase_objectives, &evidence, judge_model, dual)
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
            let combined_hash = PhaseProof::compute_combined_hash(
                phase_id,
                skill,
                &evidence_hash,
                &previous_hash,
                verdict.sufficient,
            );

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
                            "Phase '{phase_id}' cannot be advanced — prior required phases are incomplete. \
                             Phases must be completed in order."
                        );
                    }
                } else {
                    // **Attack #79 fix**: No workflow definition available — fail closed.
                    // Previously fell back to advance_unchecked() which bypasses sequential
                    // enforcement. Now refuse to advance without a workflow definition.
                    // ProofEngine callers MUST provide a workflow parameter.
                    bail!(
                        "Phase '{phase_id}' for skill '{skill}' cannot be advanced — no workflow definition provided. \
                         ProofEngine requires workflow context for sequential enforcement."
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

    /// Obtain the completion verdict for a phase. When `dual` is set, run the
    /// cross-vendor [`JudgeTrustTier::DualFrontier`] (Opus 4.8 + GPT-5.5) via
    /// `evaluate_multi` and fold the `MultiJudgeVerdict` into a single
    /// `JudgeVerdict` (conservative: `sufficient` is the AND across judges,
    /// confidence the floor — already how `synthesize` works); otherwise run
    /// the single configured `judge_model`. Folding means a wrong "done" needs
    /// BOTH frontier models to agree — Sentinel's most expensive error gets
    /// two adversarial opinions for the phases that opt in.
    async fn judge_verdict_for(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        judge_model: JudgeModel,
        dual: bool,
    ) -> Result<JudgeVerdict> {
        if !dual {
            return self
                .judge
                .evaluate(skill, phase_id, phase_objectives, evidence, judge_model)
                .await;
        }
        let multi = self
            .judge
            .evaluate_multi(
                skill,
                phase_id,
                phase_objectives,
                evidence,
                sentinel_domain::multi_judge::JudgeTrustTier::DualFrontier,
            )
            .await?;
        // Fold to a single verdict: the synthesized sufficient/confidence are
        // already conservative; concatenate the per-judge reasoning so the
        // proof records both opinions.
        let reasoning = if multi.individuals.is_empty() {
            "dual-frontier judge produced no individual verdicts".to_string()
        } else {
            multi
                .individuals
                .iter()
                .map(|run| format!("[{}] {}", run.model, run.verdict.reasoning))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(JudgeVerdict {
            sufficient: multi.sufficient,
            confidence: multi.confidence,
            reasoning,
            requested_evidence: None,
        })
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
    ///    verdicts are still observable via the `JudgeError` surface in
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
                verdict.sufficient,
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
                signature: None, // signed below via sign_with when a key is configured (#4)
                trace_context: None, // M4.5 — exporter wiring lands separately
                started_at,
                completed_at,
                duration_ms: (completed_at - started_at)
                    .num_milliseconds()
                    .unsigned_abs(),
            };

            // #4 — Ed25519 attestation. Mandatory-signing posture: refuse to
            // seal an unsigned proof when signing is required but no key is
            // configured (audit-grade must be attestable, never silently
            // unsigned). Otherwise sign when a key is present; leave unsigned
            // (hash-chain only) when neither key nor requirement is set.
            let mut proof = proof;
            match (&self.signing_key, self.signing_required) {
                (Some(key), _) => proof.sign_with(key),
                (None, true) => bail!(
                    "SENTINEL_SIGNING_REQUIRED is set but no SENTINEL_SIGNING_KEY                      is configured — refusing to seal an unsigned StepProof for                      '{phase_id}.{step_id}' (skill '{skill}'). Provide a 32-byte                      hex Ed25519 seed in SENTINEL_SIGNING_KEY, or unset                      SENTINEL_SIGNING_REQUIRED."
                ),
                (None, false) => {}
            }

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
            .ok_or_else(|| anyhow::anyhow!("No proof chain for skill '{skill}'"))?;
        let mut verification = chain.verify();

        // Fold in Ed25519 signature verification when a verify key is
        // configured. Without this, a signed entry whose combined_hash was
        // altered (or whose signature is forged) still passes the hash-only
        // chain check. Fail closed: any signature failure invalidates the chain.
        if let Some(key) = &self.verify_key {
            let report = chain.verify_signatures(key, self.signing_required);
            if !report.is_ok() {
                verification.valid = false;
                for entry_id in report.failures {
                    verification
                        .errors
                        .push(format!("signature verification failed for entry {entry_id}"));
                }
            }
        }
        // When no verify key is configured, behavior is unchanged (hash-only) —
        // back-compat. Surfacing "signatures not verified" is the display
        // layer's job (verify_cmd), not a chain error.

        Ok(verification)
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

    /// A judge whose `evaluate_multi` returns a fixed two-judge verdict (Opus +
    /// Codex), so the dual fold in `judge_verdict_for` can be exercised without
    /// the network. `evaluate` (single path) is unreachable here.
    struct DualStubJudge {
        opus_sufficient: bool,
        codex_sufficient: bool,
    }

    #[async_trait::async_trait]
    impl JudgeService for DualStubJudge {
        async fn evaluate(
            &self,
            _s: &str,
            _p: &str,
            _o: &str,
            _e: &Evidence,
            _m: JudgeModel,
        ) -> Result<JudgeVerdict> {
            unreachable!("dual path must use evaluate_multi, not evaluate")
        }

        async fn evaluate_multi(
            &self,
            _s: &str,
            _p: &str,
            _o: &str,
            _e: &Evidence,
            tier: sentinel_domain::multi_judge::JudgeTrustTier,
        ) -> Result<sentinel_domain::multi_judge::MultiJudgeVerdict> {
            use sentinel_domain::multi_judge::{JudgeRun, MultiJudgeVerdict};
            let mk = |suf: bool, conf: f64, model: JudgeModel| JudgeRun {
                model,
                verdict: if suf {
                    JudgeVerdict::pass(conf, "ok")
                } else {
                    JudgeVerdict::fail(conf, "not done", vec![])
                },
                cost_usd: None,
                provider: None,
            };
            let runs = vec![
                mk(self.opus_sufficient, 0.9, JudgeModel::Opus),
                mk(self.codex_sufficient, 0.7, JudgeModel::Codex),
            ];
            Ok(MultiJudgeVerdict::synthesize(tier, runs))
        }
    }

    fn dual_engine(opus: bool, codex: bool) -> ProofEngine {
        let state = Arc::new(RwLock::new(SessionState::new("dual-session")));
        ProofEngine::new(
            state,
            Arc::new(DualStubJudge {
                opus_sufficient: opus,
                codex_sufficient: codex,
            }),
        )
    }

    #[tokio::test]
    async fn dual_verdict_sufficient_only_when_both_agree() {
        // Both pass → sufficient, confidence = floor (0.7), reasoning names both.
        let v = dual_engine(true, true)
            .judge_verdict_for("s", "p", "o", &Evidence::default(), JudgeModel::Opus, true)
            .await
            .unwrap();
        assert!(v.sufficient);
        assert!((v.confidence - 0.7).abs() < 1e-9);
        assert!(v.reasoning.contains("opus") || v.reasoning.contains("Opus"));
    }

    #[tokio::test]
    async fn dual_verdict_fails_if_one_dissents() {
        // Opus passes, GPT-5.5 fails → NOT sufficient (conservative AND).
        let v = dual_engine(true, false)
            .judge_verdict_for("s", "p", "o", &Evidence::default(), JudgeModel::Opus, true)
            .await
            .unwrap();
        assert!(!v.sufficient, "a single dissent must fail the completion verdict");
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

    // #4 — Ed25519 attestation tests.

    #[tokio::test]
    async fn signing_key_present_produces_a_signature_on_the_sealed_proof() {
        use ed25519_dalek::{SigningKey, VerifyingKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying: VerifyingKey = key.verifying_key();
        let eng = engine().with_signing(Some(key), false);
        let proof = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "step",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Opus,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect("seals with a key");
        assert!(
            proof.signature.is_some(),
            "a configured signing key must produce a signature"
        );
        assert!(
            proof.verify_signature(&verifying).expect("verify ok"),
            "the signature must verify against the signing key's public key"
        );
    }

    #[tokio::test]
    async fn signing_required_without_a_key_refuses_to_seal() {
        // Audit-grade posture: required + no key => hard error, never an
        // unsigned (un-attestable) proof, and the chain stays unmutated.
        let eng = engine().with_signing(None, true);
        let result = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "step",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Opus,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await;
        assert!(result.is_err(), "required signing without a key must error");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("SENTINEL_SIGNING_REQUIRED") && msg.contains("unsigned"),
            "error must explain the missing-key refusal: {msg}"
        );
        let state = eng.state.read().await;
        assert!(
            state.proof_chains.get("linear").is_none(),
            "refused seal must not mutate the chain"
        );
    }

    #[tokio::test]
    async fn no_key_no_requirement_seals_unsigned_backcompat() {
        let eng = engine(); // default: no key, not required
        let proof = eng
            .submit_step_evidence(
                "linear", "claim", "1", "step",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Opus,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect("unsigned seal is the back-compat default");
        assert!(proof.signature.is_none(), "no key => unsigned (hash-chain only)");
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
                "linear",
                &evidence_hash,
                sentinel_domain::proof::GENESIS_HASH,
                true, // matches judge_verdict: JudgeVerdict::pass(..) below
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
