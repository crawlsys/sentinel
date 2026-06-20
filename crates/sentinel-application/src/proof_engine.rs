//! Proof Engine
//!
//! Manages proof chains: creates proofs from evidence, adds them to chains,
//! verifies chain integrity. Coordinates with AI judges.

use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use ed25519_dalek::{SigningKey, VerifyingKey};
use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
use sentinel_domain::proof::{PhaseProof, ProofChain};
use sentinel_domain::state::SessionState;
use sentinel_domain::step_proof::StepProof;
use sentinel_domain::workflow::{SkillWorkflow, StepStatus, WorkflowState, WorkflowStep};

use crate::judge_service::JudgeService;

#[cfg(test)]
fn test_graph_run_with_authority(mut graph_run: serde_json::Value) -> serde_json::Value {
    let graph_state = graph_run
        .get("state")
        .cloned()
        .expect("test graph run must include state");
    let obj = graph_run
        .as_object_mut()
        .expect("test graph run must be an object");
    obj.insert(
        "workflow_authority".to_string(),
        serde_json::json!("langgraph"),
    );
    obj.insert("graph_state".to_string(), graph_state);
    graph_run
}

#[cfg(test)]
fn test_step_graph_state(
    workflow_state: &WorkflowState,
    phase_id: &str,
    step_policy: &WorkflowStep,
) -> Result<serde_json::Value> {
    let mut graph_state = serde_json::to_value(workflow_state)?;
    let Some(obj) = graph_state.as_object_mut() else {
        bail!("test workflow state did not serialize to an object");
    };
    obj.insert(
        "step_policy_evidence".to_string(),
        serde_json::json!([step_policy_evidence_json(phase_id, step_policy)]),
    );
    Ok(graph_state)
}

#[cfg(test)]
fn step_policy_evidence_json(phase_id: &str, step_policy: &WorkflowStep) -> serde_json::Value {
    serde_json::json!({
        "phase_id": phase_id,
        "step_id": &step_policy.id,
        "timeout_ms": step_policy.timeout_ms,
        "retry_max_attempts": step_policy.retry_policy.max_attempts,
        "retry_backoff_ms": step_policy.retry_policy.backoff_ms,
        "retry_on": &step_policy.retry_policy.retry_on,
        "circuit_failure_threshold": step_policy.circuit_breaker.failure_threshold,
        "circuit_cooldown_ms": step_policy.circuit_breaker.cooldown_ms,
    })
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestStepGraphAuthority {
    states: std::sync::Mutex<std::collections::BTreeMap<(String, String), WorkflowState>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl StepGraphAuthority for TestStepGraphAuthority {
    async fn apply_step_status(
        &self,
        skill: &str,
        session_id: &str,
        workflow: &SkillWorkflow,
        phase_id: &str,
        step_id: &str,
        step_policy: &WorkflowStep,
        status: StepStatus,
        summary: Option<String>,
    ) -> Result<StepGraphApplyResult> {
        let mut states = self.states.lock().unwrap();
        let workflow_state = states
            .entry((skill.to_string(), session_id.to_string()))
            .or_insert_with(|| WorkflowState::new(skill, session_id));
        if let Some(existing) = workflow_state
            .step_states
            .iter()
            .find(|step| step.phase_id == phase_id && step.step_id == step_id)
            .filter(|step| matches!(step.status, StepStatus::Completed | StepStatus::Skipped))
        {
            bail!(
                "step '{step_id}' in phase '{phase_id}' for skill '{skill}' is already terminal with status '{:?}' in session '{session_id}'; replay the phase before changing sealed step state",
                existing.status
            );
        }
        workflow_state.update_step(phase_id, step_id, status, summary);
        let workflow_state = workflow_state.clone();
        let graph_state = test_step_graph_state(&workflow_state, phase_id, step_policy)?;
        let checkpoint_id = format!("test-{session_id}-{phase_id}-{step_id}");
        let phase_order: Vec<&str> = workflow
            .phases
            .iter()
            .map(|phase| phase.id.as_str())
            .collect();
        let graph_run = test_graph_run_with_authority(serde_json::json!({
            "state": graph_state.clone(),
            "latest_checkpoint": {
                "checkpoint_id": checkpoint_id,
                "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                "step_number": 1,
                "state": graph_state.clone(),
                "writes": [{
                    "node_id": phase_id,
                    "channel": "state",
                    "ts": "2026-06-17T00:00:00Z",
                }],
            },
            "checkpoints": [{
                "checkpoint_id": checkpoint_id,
                "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                "step_number": 1,
                "state": graph_state.clone(),
            }],
            "writes": [{
                "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                "checkpoint_id": checkpoint_id,
                "step_number": 1,
                "channel": "state",
                "node_id": phase_id,
                "value_json": graph_state,
            }],
            "graph_topology": test_graph_topology(skill, session_id, &phase_order),
        }));
        Ok(StepGraphApplyResult {
            workflow_state,
            graph_run,
        })
    }
}

#[cfg(test)]
fn test_step_workflow(skill: &str, phase_id: &str) -> SkillWorkflow {
    SkillWorkflow {
        skill: skill.to_string(),
        phases: vec![sentinel_domain::workflow::WorkflowPhase {
            id: phase_id.to_string(),
            file: format!("{phase_id}.md"),
            required: true,
            judge: JudgeModel::Sonnet,
            description: phase_id.to_string(),
            required_dyad: None,
        }],
        blocked_tool_prefixes: Vec::new(),
        blocked_bash_patterns: Vec::new(),
        bash_allowlist: Vec::new(),
    }
}

#[cfg(test)]
fn test_workflow_step(step_id: &str, description: &str) -> WorkflowStep {
    WorkflowStep {
        id: step_id.to_string(),
        description: description.to_string(),
        blocker: false,
        baseline_threshold: 0,
        judge: None,
        timeout_ms: None,
        retry_policy: Default::default(),
        circuit_breaker: Default::default(),
        provides: Vec::new(),
        requires: Vec::new(),
        external: Vec::new(),
        inaccessible: false,
        deprecated: None,
        r#override: None,
        extra: serde_json::Value::Null,
    }
}

#[cfg(test)]
fn test_graph_topology(skill: &str, session_id: &str, phase_order: &[&str]) -> serde_json::Value {
    let nodes: Vec<_> = phase_order
        .iter()
        .map(|phase| {
            serde_json::json!({
                "id": phase,
                "deferred": false,
                "barrier_on": [],
                "metadata": {
                    "sentinel.graph": "phase",
                    "sentinel.node": phase,
                    "sentinel.skill": skill,
                    "sentinel.phase": phase,
                    "sentinel.checkpointer_backend": "sqlite",
                    "sentinel.checkpointer_scope": "database_path::memory:",
                    "sentinel.checkpointer_tenant_scope": ""
                },
                "has_error_handler": true,
                "has_timeout_policy": true,
                "interrupt_before": false,
                "interrupt_after": true
            })
        })
        .collect();
    let mut edges = vec![serde_json::json!({
        "from": "__start__",
        "kind": "conditional",
        "to": null
    })];
    edges.extend(phase_order.iter().map(|phase| {
        serde_json::json!({
            "from": phase,
            "kind": "conditional",
            "to": null
        })
    }));
    serde_json::json!({
        "skill": skill,
        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
        "phase_order": phase_order,
        "durable_checkpointer": true,
        "checkpointer_backend": "sqlite",
        "checkpointer_scope": "database_path::memory:",
        "auto_checkpoint": true,
        "max_iterations": 100,
        "schemas": {
            "state": {
                "type": "object",
                "x-sentinel": {
                    "graph": "phase",
                    "workflow_skill": skill,
                    "authority": "langgraph"
                }
            },
            "input": {
                "type": "object"
            },
            "output": {
                "type": "object"
            },
            "context": null
        },
        "nodes": nodes,
        "edges": edges
    })
}

#[cfg(test)]
fn test_nonterminal_phase_graph_run(
    skill: &str,
    session_id: &str,
    phase_id: &str,
    custom_payload: Option<serde_json::Value>,
) -> serde_json::Value {
    let graph_state = serde_json::json!({
        "skill": skill,
        "session_id": session_id,
        "phase_order": [phase_id],
        "current_phase": 0,
        "completed_phases": [],
        "complete": false,
        "last_verdict": "fail"
    });
    let checkpoint_id = format!("checkpoint-{phase_id}");
    let mut stream = vec![
        serde_json::json!({
            "event_type": "ExecutionComplete",
            "node_id": phase_id,
            "timestamp": "2026-06-17T00:00:00Z",
            "superstep": 1,
            "payload_kind": "values",
            "payload_json": graph_state.clone()
        }),
        serde_json::json!({
            "event_type": "Checkpoint",
            "node_id": phase_id,
            "timestamp": "2026-06-17T00:00:00Z",
            "superstep": 1,
            "payload_kind": "checkpoints",
            "payload_json": {
                "checkpoint_id": checkpoint_id,
                "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                "step_number": 1,
                "source": {
                    "type": "stream_update",
                    "node": phase_id
                },
                "state": graph_state.clone()
            }
        }),
    ];
    if let Some(payload) = custom_payload {
        stream.push(serde_json::json!({
            "event_type": "Custom",
            "node_id": phase_id,
            "timestamp": "2026-06-17T00:00:00Z",
            "superstep": 1,
            "payload_kind": "custom",
            "payload_json": payload
        }));
    }

    test_graph_run_with_authority(serde_json::json!({
        "state": graph_state.clone(),
        "latest_checkpoint": {
            "checkpoint_id": checkpoint_id,
            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
            "step_number": 1,
            "state": graph_state.clone(),
            "writes": [{
                "node_id": phase_id,
                "channel": "state",
                "ts": "2026-06-17T00:00:00Z"
            }]
        },
        "checkpoints": [{
            "checkpoint_id": checkpoint_id,
            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
            "step_number": 1,
            "state": graph_state.clone()
        }],
        "writes": [{
            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
            "checkpoint_id": checkpoint_id,
            "step_number": 1,
            "channel": "state",
            "node_id": phase_id,
            "value_json": graph_state
        }],
        "graph_topology": test_graph_topology(skill, session_id, &[phase_id]),
        "stream": stream
    }))
}

/// Production authority for phase progression.
///
/// Implementations must drive the durable LangGraph phase graph and return the
/// workflow projection that should be written back to [`SessionState`]. This
/// keeps `ProofEngine` independent of filesystem/config details while making
/// graph checkpointing the only phase-advance path.
#[async_trait::async_trait]
pub trait PhaseGraphAuthority: Send + Sync {
    async fn apply_verdict(
        &self,
        skill: &str,
        session_id: &str,
        workflow: &SkillWorkflow,
        phase_id: &str,
        passed: bool,
    ) -> Result<PhaseGraphApplyResult>;
}

/// Result of applying a phase verdict through the durable graph authority.
#[derive(Debug, Clone)]
pub struct PhaseGraphApplyResult {
    pub workflow_state: WorkflowState,
    pub graph_run: serde_json::Value,
}

/// Result of a successful phase proof submission plus graph execution evidence.
#[derive(Debug, Clone)]
pub struct PhaseSubmissionReport {
    pub proof: PhaseProof,
    pub phase_graph: PhaseGraphApplyResult,
}

/// Production authority for step-status persistence.
///
/// Step completion is proof-chain state and workflow state. Implementations
/// must persist the workflow mutation through the durable LangGraph graph and
/// return the checkpoint/write evidence that proves it happened.
#[async_trait::async_trait]
pub trait StepGraphAuthority: Send + Sync {
    async fn apply_step_status(
        &self,
        skill: &str,
        session_id: &str,
        workflow: &SkillWorkflow,
        phase_id: &str,
        step_id: &str,
        step_policy: &WorkflowStep,
        status: StepStatus,
        summary: Option<String>,
    ) -> Result<StepGraphApplyResult>;
}

/// Result of applying a step status through the durable graph authority.
#[derive(Debug, Clone)]
pub struct StepGraphApplyResult {
    pub workflow_state: WorkflowState,
    pub graph_run: serde_json::Value,
}

/// Result of a successful step proof submission plus graph mutation evidence.
#[derive(Debug, Clone)]
pub struct StepSubmissionReport {
    pub proof: StepProof,
    pub step_graph: StepGraphApplyResult,
}

/// Proof engine — builds and verifies proof chains
pub struct ProofEngine {
    /// Shared session state
    state: Arc<RwLock<SessionState>>,

    /// AI judge service
    judge: Arc<dyn JudgeService>,

    /// Ed25519 signing key (#4 — proof attestation). When present,
    /// every sealed phase/step proof is signed over its `combined_hash`, so verifiers
    /// can confirm "the holder of this key authored this chain entry" — beyond
    /// the SHA-256 hash chain. Loaded from `SENTINEL_SIGNING_KEY` by the CLI
    /// layer (sentinel-domain stays pure / key-agnostic). `None` is accepted
    /// only when a test/local caller has explicitly disabled mandatory signing.
    signing_key: Option<SigningKey>,

    /// When true, sealing REFUSES to proceed without a signing key — the
    /// mandatory-attestation posture for audit-grade deployments. Set from
    /// `SENTINEL_SIGNING_REQUIRED`. With no key configured, every seal errors
    /// rather than silently writing an unsigned (un-attestable) proof.
    signing_required: bool,

    /// Ed25519 PUBLIC key for verifying signatures during chain
    /// verification. Loaded from `SENTINEL_VERIFY_KEY` by the CLI layer. When
    /// present, `verify_chain` checks every signable proof entry and fails
    /// closed on unsigned or invalid signatures. Deliberately independent of
    /// `signing_key`: deriving the verify key from the signing key would let
    /// whoever holds the signing key (potentially the agent) re-sign a forged
    /// chain. `None` makes verification invalid.
    verify_key: Option<VerifyingKey>,

    /// Durable LangGraph phase authority. Phase-level proof submission fails
    /// closed when a workflow definition is provided but this authority is not
    /// wired; direct `WorkflowState::advance_sequential` mutation is not a
    /// production alternate path.
    phase_graph: Option<Arc<dyn PhaseGraphAuthority>>,

    /// Durable LangGraph step authority. Step proof submission fails closed
    /// when this authority is not wired; direct proof-chain appends are not a
    /// production alternate path.
    step_graph: Option<Arc<dyn StepGraphAuthority>>,
}

impl ProofEngine {
    pub fn new(state: Arc<RwLock<SessionState>>, judge: Arc<dyn JudgeService>) -> Self {
        Self {
            state,
            judge,
            signing_key: None,
            signing_required: true,
            verify_key: None,
            phase_graph: None,
            step_graph: None,
        }
    }

    /// Wire an Ed25519 signing key + the mandatory-signing posture (#4).
    /// When `key` is `Some`, every sealed phase/step proof is signed. When
    /// `required` is true, sealing without a key is refused (error) — the
    /// audit-grade attestation guarantee. Builder shape; tests and local
    /// tooling can construct non-authoritative unsigned material only by
    /// explicitly disabling the required posture.
    #[must_use]
    pub fn with_signing(mut self, key: Option<SigningKey>, required: bool) -> Self {
        self.signing_key = key;
        self.signing_required = required;
        self
    }

    /// Wire the Ed25519 PUBLIC verifying key used by [`verify_chain`]. When
    /// `Some`, chain verification checks every signable proof entry and fails
    /// closed on unsigned or invalid signatures. Loaded from
    /// `SENTINEL_VERIFY_KEY`.
    #[must_use]
    pub fn with_verify_key(mut self, key: Option<VerifyingKey>) -> Self {
        self.verify_key = key;
        self
    }

    /// Wire the durable LangGraph phase authority used by
    /// [`submit_evidence`](Self::submit_evidence).
    #[must_use]
    pub fn with_phase_graph_authority(mut self, authority: Arc<dyn PhaseGraphAuthority>) -> Self {
        self.phase_graph = Some(authority);
        self
    }

    /// Wire the durable LangGraph step authority used by
    /// [`submit_step_evidence_report`](Self::submit_step_evidence_report).
    #[must_use]
    pub fn with_step_graph_authority(mut self, authority: Arc<dyn StepGraphAuthority>) -> Self {
        self.step_graph = Some(authority);
        self
    }

    /// Whether phase proof sealing has a durable LangGraph phase authority.
    ///
    /// Used by startup validators to make incomplete MCP/CLI authority wiring
    /// fail before the first production tool call.
    #[must_use]
    pub fn has_phase_graph_authority(&self) -> bool {
        self.phase_graph.is_some()
    }

    /// Whether step proof sealing has a durable LangGraph step authority.
    ///
    /// Used by startup validators to make incomplete MCP/CLI authority wiring
    /// fail before the first production tool call.
    #[must_use]
    pub fn has_step_graph_authority(&self) -> bool {
        self.step_graph.is_some()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_test_step_graph_authority(self) -> Self {
        self.with_step_graph_authority(Arc::new(TestStepGraphAuthority::default()))
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
        Ok(self
            .submit_evidence_report(
                skill,
                phase_id,
                phase_objectives,
                evidence,
                judge_model,
                started_at,
                workflow,
                dual,
            )
            .await?
            .proof)
    }

    /// Submit evidence for a phase and return the sealed proof plus durable
    /// LangGraph execution evidence.
    pub async fn submit_evidence_report(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: Evidence,
        judge_model: JudgeModel,
        started_at: chrono::DateTime<Utc>,
        workflow: Option<&sentinel_domain::workflow::SkillWorkflow>,
        dual: bool,
    ) -> Result<PhaseSubmissionReport> {
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

        let workflow = workflow.ok_or_else(|| {
            anyhow::anyhow!(
                "Phase '{phase_id}' for skill '{skill}' cannot be judged or advanced — no workflow definition provided. \
                 ProofEngine requires workflow context for LangGraph phase authority."
            )
        })?;
        if self.phase_graph.is_none() {
            bail!(
                "Phase '{phase_id}' for skill '{skill}' cannot be judged or advanced — LangGraph phase authority is not configured"
            );
        }

        // Ask AI judge to evaluate the evidence. For high-stakes (`dual`)
        // phases the verdict comes from the cross-vendor DualFrontier tier
        // (Opus 4.8 + GPT-5.5), folded conservatively into a single verdict;
        // otherwise the single configured `judge_model` runs.
        let verdict = self
            .judge_verdict_for(
                skill,
                phase_id,
                phase_objectives,
                &evidence,
                judge_model,
                dual,
            )
            .await?;

        info!(
            phase = phase_id,
            skill,
            sufficient = verdict.sufficient,
            confidence = verdict.confidence,
            "Judge verdict received"
        );

        if !verdict.sufficient {
            // Failed judge verdicts are graph state too: persist the Fail verdict
            // through LangGraph before touching local cooldown counters.
            self.apply_phase_graph_verdict(skill, phase_id, workflow, false)
                .await?;
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

        // Graph authority must accept and persist the phase transition before
        // a success proof is sealed. Otherwise an out-of-order or dyad-rejected
        // phase could leave behind a valid-looking PhaseProof for work the
        // graph refused to advance.
        let phase_graph = self
            .apply_phase_graph_verdict(skill, phase_id, workflow, true)
            .await?;

        // Compute hashes, build proof, and add to chain under a single write
        // lock to prevent TOCTOU races on concurrent submissions.
        let (proof, combined_hash) = {
            let mut state = self.state.write().await;
            let completed_at = Utc::now();

            let evidence_hash = PhaseProof::compute_evidence_hash(&evidence);
            let previous_hash = state.proof_chain_head_hash(skill).to_string();
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
                signature: None,
                started_at,
                completed_at,
                duration_ms: (completed_at - started_at)
                    .num_milliseconds()
                    .unsigned_abs(),
            };

            let mut proof = proof;
            match (&self.signing_key, self.signing_required) {
                (Some(key), _) => proof.sign_with(key),
                (None, true) => bail!(
                    "SENTINEL_SIGNING_REQUIRED is set but no SENTINEL_SIGNING_KEY \
                     is configured — refusing to seal an unsigned PhaseProof for \
                     '{phase_id}' (skill '{skill}'). Provide a 32-byte hex \
                     Ed25519 seed in SENTINEL_SIGNING_KEY."
                ),
                (None, false) => {}
            }

            // Add to chain
            state.append_phase_proof(skill, proof.clone())?;

            (proof, combined_hash)
        };

        debug!(
            phase = phase_id,
            tessera = &combined_hash[..12],
            "Proof added to chain"
        );

        Ok(PhaseSubmissionReport { proof, phase_graph })
    }

    async fn apply_phase_graph_verdict(
        &self,
        skill: &str,
        phase_id: &str,
        workflow: &SkillWorkflow,
        passed: bool,
    ) -> Result<PhaseGraphApplyResult> {
        let authority = self.phase_graph.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Phase '{phase_id}' for skill '{skill}' cannot be advanced — LangGraph phase authority is not configured"
            )
        })?;
        let session_id = {
            let state = self.state.read().await;
            state.session_id.clone()
        };
        let graph_result = authority
            .apply_verdict(skill, &session_id, workflow, phase_id, passed)
            .await?;
        Self::validate_phase_graph_apply_result(
            skill,
            &session_id,
            phase_id,
            workflow,
            &graph_result,
        )?;

        {
            let mut state = self.state.write().await;
            state.set_graph_projected_workflow(
                skill.to_string(),
                graph_result.workflow_state.clone(),
            );
        }

        Ok(graph_result)
    }

    #[cfg(test)]
    fn langgraph_tenant_scope_from_env() -> Result<Option<String>> {
        let value = match std::env::var(sentinel_domain::langgraph_thread::LANGGRAPH_TENANT_ENV) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to read {}: {err}",
                    sentinel_domain::langgraph_thread::LANGGRAPH_TENANT_ENV
                ));
            }
        };
        let tenant = value.trim();
        sentinel_domain::langgraph_thread::validate_tenant_scope(tenant)
            .map_err(anyhow::Error::msg)?;
        Ok(Some(tenant.to_string()))
    }

    #[cfg(test)]
    fn phase_checkpointer_backend_from_env() -> Result<&'static str> {
        let backend = match std::env::var("SENTINEL_PHASE_GRAPH_CHECKPOINTER") {
            Ok(value) => {
                let backend = value.trim();
                if backend.is_empty() {
                    return Err(anyhow::anyhow!(
                        "SENTINEL_PHASE_GRAPH_CHECKPOINTER is set but empty; expected sqlite, postgres, or redis"
                    ));
                }
                backend.to_ascii_lowercase()
            }
            Err(std::env::VarError::NotPresent) => return Ok("sqlite"),
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to read SENTINEL_PHASE_GRAPH_CHECKPOINTER: {err}"
                ));
            }
        };

        match backend.as_str() {
            "sqlite" => Ok("sqlite"),
            "postgres" => Ok("postgres"),
            "redis" => Ok("redis"),
            other => Err(anyhow::anyhow!(
                "unsupported phase graph checkpointer backend '{other}' from SENTINEL_PHASE_GRAPH_CHECKPOINTER; expected sqlite, postgres, or redis"
            )),
        }
    }

    #[cfg(test)]
    fn langgraph_tenant_scope_for_phase_backend_from_env() -> Result<Option<String>> {
        match Self::phase_checkpointer_backend_from_env()? {
            "sqlite" => Ok(None),
            "postgres" | "redis" => Self::langgraph_tenant_scope_from_env(),
            other => Err(anyhow::anyhow!(
                "unsupported phase graph checkpointer backend '{other}'"
            )),
        }
    }

    #[cfg(test)]
    fn expected_phase_thread_id(skill: &str, session_id: &str) -> Result<String> {
        sentinel_domain::langgraph_thread::phase_thread_id(
            skill,
            session_id,
            Self::langgraph_tenant_scope_for_phase_backend_from_env()?.as_deref(),
        )
        .map_err(anyhow::Error::msg)
    }

    /// Validate the serialized LangGraph evidence returned by the phase graph
    /// authority before Sentinel accepts its workflow projection.
    fn validate_phase_graph_apply_result(
        skill: &str,
        session_id: &str,
        phase_id: &str,
        workflow: &SkillWorkflow,
        result: &PhaseGraphApplyResult,
    ) -> Result<()> {
        let graph = result.graph_run.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph phase authority for '{skill}/{phase_id}' returned non-object graph evidence"
            )
        })?;
        let state_value = graph.get("state").ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph phase authority for '{skill}/{phase_id}' returned graph evidence without state"
            )
        })?;
        let state = state_value.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph phase authority for '{skill}/{phase_id}' returned non-object graph state"
            )
        })?;
        Self::validate_graph_authority_fields(
            graph,
            state_value,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        let graph_skill = state
            .get("skill")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted state.skill"
                )
            })?;
        if graph_skill != skill {
            bail!(
                "LangGraph phase evidence skill mismatch for '{skill}/{phase_id}': got '{graph_skill}'"
            );
        }
        let graph_session = state
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted state.session_id"
                )
            })?;
        if graph_session != session_id {
            bail!(
                "LangGraph phase evidence session mismatch for '{skill}/{phase_id}': got '{graph_session}'"
            );
        }
        Self::validate_projected_workflow_state(
            state,
            &result.workflow_state,
            skill,
            session_id,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        let completed = state
            .get("completed_phases")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted state.completed_phases"
                )
            })?;
        let completed: Vec<String> = completed
            .iter()
            .map(|value| {
                value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph phase evidence for '{skill}/{phase_id}' has non-string completed phase"
                    )
                })
            })
            .collect::<Result<_>>()?;
        if completed != result.workflow_state.completed_phases {
            bail!("LangGraph phase evidence completed_phases mismatch for '{skill}/{phase_id}'");
        }
        let graph_complete = state
            .get("complete")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted state.complete"
                )
            })?;
        if graph_complete != result.workflow_state.complete {
            bail!("LangGraph phase evidence complete flag mismatch for '{skill}/{phase_id}'");
        }
        match (
            state.get("current_phase"),
            result.workflow_state.current_phase,
        ) {
            (Some(serde_json::Value::Null) | None, None) => {}
            (Some(value), Some(expected)) if value.as_u64() == Some(expected as u64) => {}
            _ => bail!("LangGraph phase evidence current_phase mismatch for '{skill}/{phase_id}'"),
        }
        let latest_checkpoint = graph
            .get("latest_checkpoint")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted latest checkpoint"
                )
            })?;
        let checkpoint_id = latest_checkpoint
            .get("checkpoint_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted latest checkpoint id"
                )
            })?;
        if checkpoint_id.is_empty() {
            bail!("LangGraph phase evidence for '{skill}/{phase_id}' had empty checkpoint id");
        }
        let latest_checkpoint_state = latest_checkpoint.get("state").ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph phase evidence for '{skill}/{phase_id}' omitted latest checkpoint state"
            )
        })?;
        if latest_checkpoint_state != state_value {
            bail!(
                "LangGraph phase evidence latest checkpoint state mismatch for '{skill}/{phase_id}'"
            );
        }
        let checkpoints = graph
            .get("checkpoints")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' omitted checkpoint history"
                )
            })?;
        let expected_thread_id = Self::validate_graph_topology(
            graph,
            skill,
            session_id,
            phase_id,
            workflow,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        Self::validate_graph_state_phase_order(
            state,
            workflow,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        Self::validate_checkpoint_history_evidence(
            latest_checkpoint,
            checkpoints,
            checkpoint_id,
            &expected_thread_id,
            state_value,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        Self::validate_latest_checkpoint_write_evidence(
            graph,
            latest_checkpoint,
            checkpoint_id,
            &expected_thread_id,
            phase_id,
            state_value,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        let stream = graph
            .get("stream")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase authority for '{skill}/{phase_id}' returned graph evidence without stream"
                )
            })?;
        if stream.is_empty() {
            if graph_complete {
                return Ok(());
            }
            bail!(
                "LangGraph phase evidence for '{skill}/{phase_id}' has no stream for a non-terminal transition"
            );
        }

        let has_values = stream.iter().any(|part| {
            part.get("payload_kind").and_then(serde_json::Value::as_str) == Some("values")
        });
        if !has_values {
            bail!(
                "LangGraph phase evidence for '{skill}/{phase_id}' stream omitted values payload"
            );
        }
        let current_phase = result.workflow_state.current_phase.ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph phase evidence for '{skill}/{phase_id}' non-terminal stream omitted current_phase"
            )
        })?;
        let expected_gate_phase = state
            .get("phase_order")
            .and_then(serde_json::Value::as_array)
            .and_then(|phases| phases.get(current_phase))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph phase evidence for '{skill}/{phase_id}' could not resolve current gate phase from phase_order"
                )
            })?;
        Self::validate_phase_stream_checkpoint_evidence(
            stream,
            &expected_thread_id,
            expected_gate_phase,
            &result.workflow_state,
            skill,
            session_id,
            &format!("phase evidence for '{skill}/{phase_id}'"),
        )?;
        let has_custom_gate = stream.iter().any(|part| {
            let payload = part.get("payload_json");
            part.get("payload_kind").and_then(serde_json::Value::as_str) == Some("custom")
                && payload
                    .and_then(|payload| payload.get("type"))
                    .and_then(serde_json::Value::as_str)
                    == Some("sentinel.phase_gate")
                && payload
                    .and_then(|payload| payload.get("skill"))
                    .and_then(serde_json::Value::as_str)
                    == Some(skill)
                && payload
                    .and_then(|payload| payload.get("session_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some(session_id)
                && payload
                    .and_then(|payload| payload.get("phase_id"))
                    .and_then(serde_json::Value::as_str)
                    == Some(expected_gate_phase)
                && payload
                    .and_then(|payload| payload.get("phase_index"))
                    .and_then(serde_json::Value::as_u64)
                    == Some(current_phase as u64)
        });
        if !has_custom_gate {
            bail!(
                "LangGraph phase evidence for '{skill}/{phase_id}' stream omitted custom phase-gate payload for '{expected_gate_phase}'"
            );
        }
        Ok(())
    }

    fn validate_phase_stream_checkpoint_evidence(
        stream: &[serde_json::Value],
        expected_thread_id: &str,
        expected_gate_phase: &str,
        workflow_state: &WorkflowState,
        skill: &str,
        session_id: &str,
        evidence_label: &str,
    ) -> Result<()> {
        let part = stream
            .iter()
            .rev()
            .find(|part| {
                part.get("payload_kind").and_then(serde_json::Value::as_str)
                    == Some("checkpoints")
                    && part.get("node_id").and_then(serde_json::Value::as_str)
                        == Some(expected_gate_phase)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream omitted checkpoint payload for gate '{expected_gate_phase}'"
                )
            })?;
        let stream_node = part
            .get("node_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint payload omitted node_id"
                )
            })?;
        if stream_node != expected_gate_phase {
            bail!(
                "LangGraph {evidence_label} stream checkpoint node mismatch: got '{stream_node}', expected '{expected_gate_phase}'"
            );
        }
        let payload = part
            .get("payload_json")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint payload was not an object"
                )
            })?;
        let checkpoint_id = payload
            .get("checkpoint_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint for gate '{expected_gate_phase}' omitted checkpoint_id"
                )
            })?;
        if checkpoint_id.is_empty() {
            bail!(
                "LangGraph {evidence_label} stream checkpoint for gate '{expected_gate_phase}' had empty checkpoint_id"
            );
        }
        let stream_thread = payload
            .get("thread_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' omitted thread_id"
                )
            })?;
        if stream_thread != expected_thread_id {
            bail!(
                "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' thread mismatch: got '{stream_thread}', expected '{expected_thread_id}'"
            );
        }
        payload
            .get("step_number")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' omitted numeric step_number"
                )
            })?;
        let source_type = payload
            .get("source")
            .and_then(serde_json::Value::as_object)
            .and_then(|source| source.get("type"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' omitted source type"
                )
            })?;
        if source_type != "stream_update" {
            bail!(
                "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' source type mismatch: got '{source_type}', expected 'stream_update'"
            );
        }
        let source_node = payload
            .get("source")
            .and_then(serde_json::Value::as_object)
            .and_then(|source| source.get("node"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' omitted source node"
                )
            })?;
        if source_node != expected_gate_phase {
            bail!(
                "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' source node mismatch: got '{source_node}', expected '{expected_gate_phase}'"
            );
        }
        let stream_state = payload.get("state").ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' omitted state"
            )
        })?;
        let stream_state = stream_state.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph {evidence_label} stream checkpoint '{checkpoint_id}' state was not an object"
            )
        })?;
        Self::validate_projected_workflow_state(
            stream_state,
            workflow_state,
            skill,
            session_id,
            evidence_label,
        )?;

        Ok(())
    }

    fn validate_graph_topology(
        graph: &serde_json::Map<String, serde_json::Value>,
        skill: &str,
        session_id: &str,
        phase_id: &str,
        workflow: &SkillWorkflow,
        evidence_label: &str,
    ) -> Result<String> {
        if workflow.skill != skill {
            bail!(
                "LangGraph {evidence_label} workflow definition skill mismatch: got '{}', expected '{skill}'",
                workflow.skill
            );
        }
        let topology = graph
            .get("graph_topology")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} omitted compiled graph topology")
            })?;
        let topology_skill = topology
            .get("skill")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("LangGraph {evidence_label} topology omitted skill"))?;
        if topology_skill != skill {
            bail!("LangGraph {evidence_label} topology skill mismatch: got '{topology_skill}'");
        }
        let thread_id = topology
            .get("thread_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted thread_id")
            })?;
        if topology
            .get("durable_checkpointer")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            bail!("LangGraph {evidence_label} topology did not prove a durable checkpointer");
        }
        if topology
            .get("auto_checkpoint")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            bail!("LangGraph {evidence_label} topology did not prove LangGraph auto-checkpointing");
        }
        let phase_order = topology
            .get("phase_order")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted phase_order")
            })?;
        if phase_order.is_empty() || phase_order.iter().any(|phase| phase.as_str().is_none()) {
            bail!("LangGraph {evidence_label} topology had invalid phase_order");
        }
        if !phase_order
            .iter()
            .any(|phase| phase.as_str() == Some(phase_id))
        {
            bail!("LangGraph {evidence_label} topology phase_order omitted phase '{phase_id}'");
        }
        let phase_ids: Vec<&str> = phase_order
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        let workflow_phase_ids: Vec<&str> = workflow
            .phases
            .iter()
            .map(|phase| phase.id.as_str())
            .collect();
        if phase_ids != workflow_phase_ids {
            bail!(
                "LangGraph {evidence_label} topology phase_order mismatch: got {:?}, expected {:?}",
                phase_ids,
                workflow_phase_ids
            );
        }
        let max_iterations = topology
            .get("max_iterations")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted max_iterations")
            })?;
        if max_iterations <= phase_ids.len() as u64 {
            bail!(
                "LangGraph {evidence_label} topology max_iterations {max_iterations} does not leave phase routing headroom"
            );
        }
        let schemas = topology
            .get("schemas")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted schemas")
            })?;
        let state_schema = schemas
            .get("state")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted state schema")
            })?;
        let schema_marker = |key: &str| {
            state_schema
                .get("x-sentinel")
                .and_then(serde_json::Value::as_object)
                .and_then(|marker| marker.get(key))
                .and_then(serde_json::Value::as_str)
        };
        if schema_marker("graph") != Some("phase") {
            bail!("LangGraph {evidence_label} topology state schema did not prove phase graph identity");
        }
        if schema_marker("workflow_skill") != Some(skill) {
            bail!("LangGraph {evidence_label} topology state schema workflow skill mismatch");
        }
        if schema_marker("authority") != Some("langgraph") {
            bail!("LangGraph {evidence_label} topology state schema did not prove LangGraph authority");
        }
        if schemas.get("input").is_none_or(serde_json::Value::is_null) {
            bail!("LangGraph {evidence_label} topology omitted input schema");
        }
        if schemas.get("output").is_none_or(serde_json::Value::is_null) {
            bail!("LangGraph {evidence_label} topology omitted output schema");
        }
        let backend = topology
            .get("checkpointer_backend")
            .and_then(serde_json::Value::as_str)
            .filter(|backend| !backend.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted checkpointer backend")
            })?;
        let scope = topology
            .get("checkpointer_scope")
            .and_then(serde_json::Value::as_str)
            .filter(|scope| !scope.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} topology omitted checkpointer scope")
            })?;
        let topology_tenant_scope = topology
            .get("checkpointer_tenant_scope")
            .and_then(serde_json::Value::as_str);
        let tenant_scope = match backend {
            "sqlite" => {
                if topology_tenant_scope.is_some_and(|tenant| !tenant.is_empty()) {
                    bail!(
                        "LangGraph {evidence_label} SQLite topology must not carry hosted tenant metadata"
                    );
                }
                None
            }
            "postgres" | "redis" => {
                let tenant = topology_tenant_scope
                    .filter(|tenant| !tenant.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "LangGraph {evidence_label} {backend} topology omitted checkpointer tenant scope"
                        )
                    })?;
                sentinel_domain::langgraph_thread::validate_tenant_scope(tenant)
                    .map_err(anyhow::Error::msg)?;
                Some(tenant)
            }
            other => {
                bail!(
                    "LangGraph {evidence_label} topology used unsupported checkpointer backend '{other}'"
                );
            }
        };
        let expected_thread_id =
            sentinel_domain::langgraph_thread::phase_thread_id(skill, session_id, tenant_scope)
                .map_err(anyhow::Error::msg)?;
        if thread_id != expected_thread_id {
            bail!(
                "LangGraph {evidence_label} topology thread mismatch: got '{thread_id}', expected '{expected_thread_id}'"
            );
        }
        let expected_tenant_scope_metadata = tenant_scope.unwrap_or("");
        let nodes = topology
            .get("nodes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("LangGraph {evidence_label} topology omitted nodes"))?;
        if nodes.is_empty() {
            bail!("LangGraph {evidence_label} topology had no nodes");
        }
        let mut seen_node_ids = std::collections::BTreeSet::new();
        for node in nodes {
            let node_id = node
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("LangGraph {evidence_label} topology node omitted id")
                })?;
            if !phase_ids.contains(&node_id) {
                bail!("LangGraph {evidence_label} topology had unexpected node '{node_id}'");
            }
            if !seen_node_ids.insert(node_id) {
                bail!("LangGraph {evidence_label} topology duplicated node '{node_id}'");
            }
        }
        for phase in &phase_ids {
            let node = nodes
                .iter()
                .find(|node| node.get("id").and_then(serde_json::Value::as_str) == Some(*phase))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} topology omitted phase node '{phase}'"
                    )
                })?;
            let metadata = node
                .get("metadata")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} topology node '{phase}' omitted metadata"
                    )
                })?;
            let metadata_value = |key: &str| metadata.get(key).and_then(serde_json::Value::as_str);
            if metadata_value("sentinel.graph") != Some("phase") {
                bail!(
                    "LangGraph {evidence_label} topology node '{phase}' did not prove phase graph identity"
                );
            }
            if metadata_value("sentinel.node") != Some(*phase) {
                bail!("LangGraph {evidence_label} topology node '{phase}' metadata node mismatch");
            }
            if metadata_value("sentinel.skill") != Some(skill) {
                bail!("LangGraph {evidence_label} topology node '{phase}' metadata skill mismatch");
            }
            if metadata_value("sentinel.phase") != Some(*phase) {
                bail!("LangGraph {evidence_label} topology node '{phase}' metadata phase mismatch");
            }
            if metadata_value("sentinel.checkpointer_backend") != Some(backend) {
                bail!(
                    "LangGraph {evidence_label} topology node '{phase}' omitted matching checkpointer backend metadata"
                );
            }
            if metadata_value("sentinel.checkpointer_scope") != Some(scope) {
                bail!(
                    "LangGraph {evidence_label} topology node '{phase}' omitted matching checkpointer scope metadata"
                );
            }
            if metadata_value("sentinel.checkpointer_tenant_scope")
                != Some(expected_tenant_scope_metadata)
            {
                bail!(
                    "LangGraph {evidence_label} topology node '{phase}' omitted matching checkpointer tenant scope metadata"
                );
            }
            if node
                .get("has_error_handler")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                bail!("LangGraph {evidence_label} topology node '{phase}' omitted error handler");
            }
            if node
                .get("has_timeout_policy")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                bail!("LangGraph {evidence_label} topology node '{phase}' omitted timeout policy");
            }
            if node
                .get("interrupt_before")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                bail!(
                    "LangGraph {evidence_label} topology node '{phase}' must not interrupt before execution"
                );
            }
            if node
                .get("interrupt_after")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                bail!(
                    "LangGraph {evidence_label} topology node '{phase}' omitted post-node interrupt"
                );
            }
        }
        let edges = topology
            .get("edges")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("LangGraph {evidence_label} topology omitted edges"))?;
        if edges.is_empty() {
            bail!("LangGraph {evidence_label} topology had no edges");
        }
        let is_start_source = |source: &str| source == "START" || source == "__start__";
        let start_count = edges
            .iter()
            .filter(|edge| {
                edge.get("from")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(is_start_source)
                    && edge.get("kind").and_then(serde_json::Value::as_str) == Some("conditional")
            })
            .count();
        if start_count == 0 {
            bail!("LangGraph {evidence_label} topology omitted conditional routing from 'START'");
        }
        if start_count > 1 {
            bail!(
                "LangGraph {evidence_label} topology duplicated conditional routing from 'START'"
            );
        }
        let required_sources: Vec<&str> = phase_ids.to_vec();
        for source in &required_sources {
            let count = edges
                .iter()
                .filter(|edge| {
                    edge.get("from").and_then(serde_json::Value::as_str) == Some(*source)
                        && edge.get("kind").and_then(serde_json::Value::as_str)
                            == Some("conditional")
                })
                .count();
            if count == 0 {
                bail!(
                    "LangGraph {evidence_label} topology omitted conditional routing from '{source}'"
                );
            }
            if count > 1 {
                bail!(
                    "LangGraph {evidence_label} topology duplicated conditional routing from '{source}'"
                );
            }
        }
        for edge in edges {
            let source = edge
                .get("from")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("LangGraph {evidence_label} topology edge omitted source")
                })?;
            if !is_start_source(source) && !required_sources.contains(&source) {
                bail!("LangGraph {evidence_label} topology edge had unexpected source '{source}'");
            }
            if edge.get("kind").and_then(serde_json::Value::as_str) != Some("conditional") {
                bail!(
                    "LangGraph {evidence_label} topology edge from '{source}' was not conditional"
                );
            }
        }
        Ok(expected_thread_id)
    }

    fn validate_graph_state_phase_order(
        state: &serde_json::Map<String, serde_json::Value>,
        workflow: &SkillWorkflow,
        evidence_label: &str,
    ) -> Result<()> {
        let Some(phase_order) = state.get("phase_order") else {
            return Ok(());
        };
        let phase_order = phase_order.as_array().ok_or_else(|| {
            anyhow::anyhow!("LangGraph {evidence_label} state.phase_order was not an array")
        })?;
        let phase_order: Vec<&str> = phase_order
            .iter()
            .map(|phase| {
                phase.as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} state.phase_order contained a non-string phase"
                    )
                })
            })
            .collect::<Result<_>>()?;
        let workflow_phase_ids: Vec<&str> = workflow
            .phases
            .iter()
            .map(|phase| phase.id.as_str())
            .collect();
        if phase_order != workflow_phase_ids {
            bail!(
                "LangGraph {evidence_label} state.phase_order mismatch: got {:?}, expected {:?}",
                phase_order,
                workflow_phase_ids
            );
        }
        Ok(())
    }

    fn validate_projected_workflow_state(
        state: &serde_json::Map<String, serde_json::Value>,
        workflow_state: &WorkflowState,
        skill: &str,
        session_id: &str,
        evidence_label: &str,
    ) -> Result<()> {
        if workflow_state.skill != skill {
            bail!(
                "LangGraph {evidence_label} workflow projection skill mismatch: got '{}', expected '{skill}'",
                workflow_state.skill
            );
        }
        if workflow_state.session_id != session_id {
            bail!(
                "LangGraph {evidence_label} workflow projection session mismatch: got '{}', expected '{session_id}'",
                workflow_state.session_id
            );
        }

        let projected_current_phase = match state.get("current_phase") {
            Some(serde_json::Value::Null) | None => None,
            Some(value) => {
                let phase = value.as_u64().ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} state.current_phase was not a non-negative integer"
                    )
                })?;
                Some(usize::try_from(phase).map_err(|_| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} state.current_phase exceeded platform usize"
                    )
                })?)
            }
        };
        if projected_current_phase != workflow_state.current_phase {
            bail!("LangGraph {evidence_label} workflow projection current_phase mismatch");
        }

        let state_completed = state
            .get("completed_phases")
            .and_then(serde_json::Value::as_array)
            .map(|completed| {
                completed
                    .iter()
                    .map(|phase| {
                        phase.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                            anyhow::anyhow!(
                                "LangGraph {evidence_label} state.completed_phases contained a non-string phase"
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        if state_completed != workflow_state.completed_phases {
            bail!("LangGraph {evidence_label} workflow projection completed_phases mismatch");
        }

        let state_complete = state
            .get("complete")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if state_complete != workflow_state.complete {
            bail!("LangGraph {evidence_label} workflow projection complete flag mismatch");
        }

        let expected_step_states = serde_json::to_value(&workflow_state.step_states)?;
        match state.get("step_states") {
            Some(step_states) if step_states == &expected_step_states => {}
            Some(_) => bail!("LangGraph {evidence_label} workflow projection step_states mismatch"),
            None if workflow_state.step_states.is_empty() => {}
            None => bail!("LangGraph {evidence_label} state omitted projected step_states"),
        }

        let expected_current_step = serde_json::to_value(&workflow_state.current_step)?;
        match state.get("current_step") {
            Some(current_step) if current_step == &expected_current_step => {}
            Some(_) => {
                bail!("LangGraph {evidence_label} workflow projection current_step mismatch")
            }
            None if workflow_state.current_step.is_none() => {}
            None => bail!("LangGraph {evidence_label} state omitted projected current_step"),
        }

        let expected_dyad_verdicts = serde_json::to_value(&workflow_state.dyad_verdicts)?;
        match state.get("dyad_verdicts") {
            Some(dyad_verdicts) if dyad_verdicts == &expected_dyad_verdicts => {}
            Some(_) => {
                bail!("LangGraph {evidence_label} workflow projection dyad_verdicts mismatch");
            }
            None if workflow_state.dyad_verdicts.is_empty() => {}
            None => bail!("LangGraph {evidence_label} state omitted projected dyad_verdicts"),
        }

        Ok(())
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

    /// Direct step submission is a guarded wrapper, not a production write path.
    ///
    /// Step completion must be accepted by the durable LangGraph step
    /// authority before a `StepProof` is appended. Call
    /// [`submit_step_evidence_report`](Self::submit_step_evidence_report)
    /// with workflow context instead.
    pub async fn submit_step_evidence(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        step_description: &str,
        _evidence: Evidence,
        _verdict: JudgeVerdict,
        _judge_model: JudgeModel,
        _artifact: serde_json::Value,
        _account_context: Option<String>,
        _started_at: chrono::DateTime<Utc>,
    ) -> Result<StepProof> {
        #[cfg(test)]
        {
            let workflow = test_step_workflow(skill, phase_id);
            let step_policy = test_workflow_step(step_id, step_description);
            Ok(self
                .submit_step_evidence_report(
                    skill,
                    phase_id,
                    step_id,
                    step_description,
                    _evidence,
                    _verdict,
                    _judge_model,
                    _artifact,
                    _account_context,
                    _started_at,
                    &workflow,
                    &step_policy,
                    Some(step_description.to_string()),
                )
                .await?
                .proof)
        }
        #[cfg(not(test))]
        {
            let _ = (
                step_description,
                _evidence,
                _verdict,
                _judge_model,
                _artifact,
                _account_context,
                _started_at,
            );
            bail!(
                "Step '{phase_id}.{step_id}' (skill '{skill}') cannot be sealed through the direct non-graph path — durable LangGraph step authority and workflow context are required"
            )
        }
    }

    /// Submit a verdict for a single step and return the sealed proof plus
    /// durable LangGraph checkpoint/write evidence.
    pub async fn submit_step_evidence_report(
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
        workflow: &SkillWorkflow,
        step_policy: &WorkflowStep,
        summary: Option<String>,
    ) -> Result<StepSubmissionReport> {
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

        let authority = self.step_graph.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Step '{phase_id}.{step_id}' for skill '{skill}' cannot be completed — LangGraph step authority is not configured"
            )
        })?;

        let (proof, step_graph) = {
            let mut state = self.state.write().await;
            let completed_at = Utc::now();
            let session_id = state.session_id.clone();

            let evidence_hash = StepProof::compute_evidence_hash(&evidence);
            let artifact_hash = StepProof::compute_artifact_hash(&artifact);
            let previous_hash = state.proof_chain_head_hash(skill).to_string();
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
            // unsigned). Tests/local callers may disable the posture
            // explicitly, but verification still rejects unsigned chains.
            let mut proof = proof;
            match (&self.signing_key, self.signing_required) {
                (Some(key), _) => proof.sign_with(key),
                (None, true) => bail!(
                    "SENTINEL_SIGNING_REQUIRED is set but no SENTINEL_SIGNING_KEY                      is configured — refusing to seal an unsigned StepProof for                      '{phase_id}.{step_id}' (skill '{skill}'). Provide a 32-byte                      hex Ed25519 seed in SENTINEL_SIGNING_KEY, or unset                      SENTINEL_SIGNING_REQUIRED."
                ),
                (None, false) => {}
            }

            let mut validation_chain = state
                .proof_chain(skill)
                .cloned()
                .unwrap_or_else(|| ProofChain::new(skill, &session_id));
            validation_chain.add_step_proof(proof.clone())?;

            let step_graph = authority
                .apply_step_status(
                    skill,
                    &session_id,
                    workflow,
                    phase_id,
                    step_id,
                    step_policy,
                    StepStatus::Completed,
                    summary,
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "LangGraph step authority failed for '{skill}/{phase_id}.{step_id}': {e:#}"
                    )
                })?;
            Self::validate_step_graph_apply_result(
                skill,
                &session_id,
                phase_id,
                step_id,
                workflow,
                step_policy,
                &step_graph,
            )?;

            state.append_step_proof(skill, proof.clone())?;
            state
                .set_graph_projected_workflow(skill.to_string(), step_graph.workflow_state.clone());

            debug!(
                skill,
                phase = phase_id,
                step = step_id,
                tessera = &combined_hash[..12],
                "StepProof added to chain"
            );

            (proof, step_graph)
        };

        Ok(StepSubmissionReport { proof, step_graph })
    }

    /// Validate the serialized LangGraph evidence returned by the step graph
    /// authority before Sentinel accepts its workflow projection.
    fn validate_step_graph_apply_result(
        skill: &str,
        session_id: &str,
        phase_id: &str,
        step_id: &str,
        workflow: &SkillWorkflow,
        step_policy: &WorkflowStep,
        result: &StepGraphApplyResult,
    ) -> Result<()> {
        let graph = result.graph_run.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph step authority for '{skill}/{phase_id}.{step_id}' returned non-object graph evidence"
            )
        })?;
        let state_value = graph.get("state").ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph step authority for '{skill}/{phase_id}.{step_id}' returned graph evidence without state"
            )
        })?;
        let state = state_value.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "LangGraph step authority for '{skill}/{phase_id}.{step_id}' returned non-object graph state"
            )
        })?;
        Self::validate_graph_authority_fields(
            graph,
            state_value,
            &format!("step evidence for '{skill}/{phase_id}.{step_id}'"),
        )?;
        let graph_skill = state
            .get("skill")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted state.skill"
                )
            })?;
        if graph_skill != skill {
            bail!(
                "LangGraph step evidence skill mismatch for '{skill}/{phase_id}.{step_id}': got '{graph_skill}'"
            );
        }
        let graph_session = state
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted state.session_id"
                )
            })?;
        if graph_session != session_id {
            bail!(
                "LangGraph step evidence session mismatch for '{skill}/{phase_id}.{step_id}': got '{graph_session}'"
            );
        }
        Self::validate_projected_workflow_state(
            state,
            &result.workflow_state,
            skill,
            session_id,
            &format!("step evidence for '{skill}/{phase_id}.{step_id}'"),
        )?;
        let step_states = state
            .get("step_states")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted state.step_states"
                )
            })?;
        let completed_step = step_states.iter().any(|step| {
            step.get("phase_id").and_then(serde_json::Value::as_str) == Some(phase_id)
                && step.get("step_id").and_then(serde_json::Value::as_str) == Some(step_id)
                && step.get("status").and_then(serde_json::Value::as_str) == Some("completed")
        });
        if !completed_step {
            bail!(
                "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' did not record the completed step"
            );
        }
        let projected_step = result.workflow_state.step_states.iter().any(|step| {
            step.phase_id == phase_id
                && step.step_id == step_id
                && step.status == StepStatus::Completed
        });
        if !projected_step {
            bail!(
                "LangGraph step projection for '{skill}/{phase_id}.{step_id}' did not include the completed step"
            );
        }
        Self::validate_step_policy_evidence(state, skill, phase_id, step_id, step_policy)?;
        let latest_checkpoint = graph
            .get("latest_checkpoint")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted latest checkpoint"
                )
            })?;
        let checkpoint_id = latest_checkpoint
            .get("checkpoint_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted latest checkpoint id"
                )
            })?;
        if checkpoint_id.is_empty() {
            bail!(
                "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' had empty checkpoint id"
            );
        }
        let latest_checkpoint_state = latest_checkpoint
            .get("state")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted latest checkpoint state"
                )
            })?;
        if latest_checkpoint_state != state_value {
            bail!(
                "LangGraph step evidence latest checkpoint state mismatch for '{skill}/{phase_id}.{step_id}'"
            );
        }
        let checkpoints = graph
            .get("checkpoints")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted checkpoint history"
                )
            })?;
        let expected_thread_id = Self::validate_graph_topology(
            graph,
            skill,
            session_id,
            phase_id,
            workflow,
            &format!("step evidence for '{skill}/{phase_id}.{step_id}'"),
        )?;
        Self::validate_graph_state_phase_order(
            state,
            workflow,
            &format!("step evidence for '{skill}/{phase_id}.{step_id}'"),
        )?;
        Self::validate_checkpoint_history_evidence(
            latest_checkpoint,
            checkpoints,
            checkpoint_id,
            &expected_thread_id,
            state_value,
            &format!("step evidence for '{skill}/{phase_id}.{step_id}'"),
        )?;
        Self::validate_latest_checkpoint_write_evidence(
            graph,
            latest_checkpoint,
            checkpoint_id,
            &expected_thread_id,
            phase_id,
            state_value,
            &format!("step evidence for '{skill}/{phase_id}.{step_id}'"),
        )?;
        Ok(())
    }

    fn validate_step_policy_evidence(
        state: &serde_json::Map<String, serde_json::Value>,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        step_policy: &WorkflowStep,
    ) -> Result<()> {
        if step_policy.id != step_id {
            bail!(
                "Configured step policy id '{}' does not match '{skill}/{phase_id}.{step_id}'",
                step_policy.id
            );
        }

        let policies = state
            .get("step_policy_evidence")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' omitted state.step_policy_evidence"
                )
            })?;
        let mut matches = policies.iter().filter(|policy| {
            policy.get("phase_id").and_then(serde_json::Value::as_str) == Some(phase_id)
                && policy.get("step_id").and_then(serde_json::Value::as_str) == Some(step_id)
        });
        let Some(policy) = matches.next() else {
            bail!(
                "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' did not record configured step policy"
            );
        };
        if matches.next().is_some() {
            bail!(
                "LangGraph step evidence for '{skill}/{phase_id}.{step_id}' recorded duplicate step policy evidence"
            );
        }

        let field_u64 = |field: &str| -> Result<u64> {
            policy
                .get(field)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph step policy evidence for '{skill}/{phase_id}.{step_id}' omitted numeric {field}"
                    )
                })
        };

        match step_policy.timeout_ms {
            Some(expected)
                if policy.get("timeout_ms").and_then(serde_json::Value::as_u64)
                    != Some(expected) =>
            {
                bail!(
                    "LangGraph step policy evidence timeout mismatch for '{skill}/{phase_id}.{step_id}'"
                );
            }
            None if policy
                .get("timeout_ms")
                .is_some_and(|value| !value.is_null()) =>
            {
                bail!(
                    "LangGraph step policy evidence timeout mismatch for '{skill}/{phase_id}.{step_id}'"
                );
            }
            _ => {}
        }
        if field_u64("retry_max_attempts")? != u64::from(step_policy.retry_policy.max_attempts) {
            bail!(
                "LangGraph step policy evidence retry_max_attempts mismatch for '{skill}/{phase_id}.{step_id}'"
            );
        }
        if field_u64("retry_backoff_ms")? != step_policy.retry_policy.backoff_ms {
            bail!(
                "LangGraph step policy evidence retry_backoff_ms mismatch for '{skill}/{phase_id}.{step_id}'"
            );
        }
        let retry_on = policy
            .get("retry_on")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph step policy evidence for '{skill}/{phase_id}.{step_id}' omitted retry_on"
                )
            })?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(std::string::ToString::to_string)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "LangGraph step policy evidence retry_on for '{skill}/{phase_id}.{step_id}' contained a non-string value"
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        if retry_on.as_slice() != step_policy.retry_policy.retry_on.as_slice() {
            bail!(
                "LangGraph step policy evidence retry_on mismatch for '{skill}/{phase_id}.{step_id}'"
            );
        }
        if field_u64("circuit_failure_threshold")?
            != u64::from(step_policy.circuit_breaker.failure_threshold)
        {
            bail!(
                "LangGraph step policy evidence circuit_failure_threshold mismatch for '{skill}/{phase_id}.{step_id}'"
            );
        }
        if field_u64("circuit_cooldown_ms")? != step_policy.circuit_breaker.cooldown_ms {
            bail!(
                "LangGraph step policy evidence circuit_cooldown_ms mismatch for '{skill}/{phase_id}.{step_id}'"
            );
        }
        Ok(())
    }

    fn validate_checkpoint_history_evidence(
        latest_checkpoint: &serde_json::Map<String, serde_json::Value>,
        checkpoints: &[serde_json::Value],
        checkpoint_id: &str,
        expected_thread_id: &str,
        state_value: &serde_json::Value,
        evidence_label: &str,
    ) -> Result<()> {
        if checkpoints.is_empty() {
            bail!("LangGraph {evidence_label} had empty checkpoint history");
        }

        let latest_thread = latest_checkpoint
            .get("thread_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} latest checkpoint omitted thread_id")
            })?;
        if latest_thread != expected_thread_id {
            bail!(
                "LangGraph {evidence_label} latest checkpoint thread mismatch: got '{latest_thread}', expected '{expected_thread_id}'"
            );
        }
        let latest_step = latest_checkpoint
            .get("step_number")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} latest checkpoint omitted numeric step_number"
                )
            })?;

        let mut previous_step = None;
        let mut previous_checkpoint_id = None;
        for checkpoint in checkpoints {
            let checkpoint = checkpoint.as_object().ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} checkpoint history contained a non-object checkpoint"
                )
            })?;
            let history_checkpoint_id = checkpoint
                .get("checkpoint_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} history checkpoint omitted checkpoint_id"
                    )
                })?;
            let history_thread = checkpoint
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} history checkpoint '{history_checkpoint_id}' omitted thread_id"
                    )
                })?;
            if history_thread != expected_thread_id {
                bail!(
                    "LangGraph {evidence_label} checkpoint history contains thread '{history_thread}', expected '{expected_thread_id}'"
                );
            }
            let history_step = checkpoint
                .get("step_number")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} history checkpoint '{history_checkpoint_id}' omitted numeric step_number"
                    )
                })?;
            if let Some(previous_step) = previous_step {
                if previous_step > history_step {
                    bail!(
                        "LangGraph {evidence_label} checkpoint history is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                        previous_checkpoint_id.unwrap_or("<missing>"),
                        previous_step,
                        history_checkpoint_id,
                        history_step
                    );
                }
            }
            previous_step = Some(history_step);
            previous_checkpoint_id = Some(history_checkpoint_id);
        }

        let history_latest = checkpoints
            .last()
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} ended with a non-object checkpoint")
            })?;
        let history_latest_id = history_latest
            .get("checkpoint_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} latest history checkpoint omitted checkpoint_id"
                )
            })?;
        if history_latest_id != checkpoint_id {
            bail!(
                "LangGraph {evidence_label} latest checkpoint mismatch: latest_checkpoint={checkpoint_id}, history_latest={history_latest_id}"
            );
        }
        let history_latest_step = history_latest
            .get("step_number")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} latest history checkpoint omitted numeric step_number"
                )
            })?;
        if history_latest_step != latest_step {
            bail!(
                "LangGraph {evidence_label} latest checkpoint step mismatch: latest_checkpoint={latest_step}, history_latest={history_latest_step}"
            );
        }
        if history_latest.get("state") != Some(state_value) {
            bail!("LangGraph {evidence_label} latest history state mismatch");
        }

        Ok(())
    }

    fn validate_graph_authority_fields(
        graph: &serde_json::Map<String, serde_json::Value>,
        state_value: &serde_json::Value,
        evidence_label: &str,
    ) -> Result<()> {
        let authority = graph
            .get("workflow_authority")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("LangGraph {evidence_label} omitted workflow_authority")
            })?;
        if authority != "langgraph" {
            bail!(
                "LangGraph {evidence_label} workflow_authority mismatch: got '{authority}', expected 'langgraph'"
            );
        }
        let graph_state = graph
            .get("graph_state")
            .ok_or_else(|| anyhow::anyhow!("LangGraph {evidence_label} omitted graph_state"))?;
        if graph_state != state_value {
            bail!("LangGraph {evidence_label} graph_state mismatch");
        }
        Ok(())
    }

    fn validate_latest_checkpoint_write_evidence(
        graph: &serde_json::Map<String, serde_json::Value>,
        latest_checkpoint: &serde_json::Map<String, serde_json::Value>,
        checkpoint_id: &str,
        expected_thread_id: &str,
        expected_node_id: &str,
        state_value: &serde_json::Value,
        evidence_label: &str,
    ) -> Result<()> {
        let checkpoint_writes = latest_checkpoint
            .get("writes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "LangGraph {evidence_label} latest checkpoint omitted write metadata"
                )
            })?;
        let state_checkpoint_writes: Vec<_> = checkpoint_writes
            .iter()
            .filter(|write| {
                write.get("channel").and_then(serde_json::Value::as_str) == Some("state")
            })
            .collect();
        if state_checkpoint_writes.is_empty() {
            bail!(
                "LangGraph {evidence_label} latest checkpoint omitted state-channel write metadata"
            );
        }
        if !state_checkpoint_writes.iter().any(|write| {
            write.get("node_id").and_then(serde_json::Value::as_str) == Some(expected_node_id)
        }) {
            let observed = state_checkpoint_writes
                .iter()
                .filter_map(|write| write.get("node_id").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "LangGraph {evidence_label} latest checkpoint state write was attributed to [{observed}], expected phase node '{expected_node_id}'"
            );
        }

        let writes = graph
            .get("writes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("LangGraph {evidence_label} omitted write history"))?;
        if let Some(mismatched) = writes.iter().find(|write| {
            write.get("thread_id").and_then(serde_json::Value::as_str) != Some(expected_thread_id)
        }) {
            let thread_id = mismatched
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<missing>");
            bail!(
                "LangGraph {evidence_label} write history contains thread '{thread_id}', expected '{expected_thread_id}'"
            );
        }
        for pair in writes.windows(2) {
            let previous_step = pair[0]
                .get("step_number")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} write history omitted numeric step_number"
                    )
                })?;
            let next_step = pair[1]
                .get("step_number")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "LangGraph {evidence_label} write history omitted numeric step_number"
                    )
                })?;
            if previous_step > next_step {
                let previous_checkpoint = pair[0]
                    .get("checkpoint_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<missing>");
                let next_checkpoint = pair[1]
                    .get("checkpoint_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<missing>");
                bail!(
                    "LangGraph {evidence_label} write history is not oldest-first: checkpoint '{previous_checkpoint}' step {previous_step} appears before checkpoint '{next_checkpoint}' step {next_step}"
                );
            }
        }
        let state_write_records_graph_state = writes.iter().any(|write| {
            write.get("thread_id").and_then(serde_json::Value::as_str) == Some(expected_thread_id)
                && write
                    .get("checkpoint_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(checkpoint_id)
                && write.get("channel").and_then(serde_json::Value::as_str) == Some("state")
                && write.get("node_id").and_then(serde_json::Value::as_str)
                    == Some(expected_node_id)
                && write.get("value_json") == Some(state_value)
        });
        if !state_write_records_graph_state {
            bail!(
                "LangGraph {evidence_label} omitted a latest-checkpoint state-channel write from phase node '{expected_node_id}' containing the accepted graph state"
            );
        }
        Ok(())
    }

    /// Verify a skill's proof chain
    pub async fn verify_chain(
        &self,
        skill: &str,
    ) -> Result<sentinel_domain::proof::ChainVerification> {
        let state = self.state.read().await;
        let chain = state
            .proof_chain(skill)
            .ok_or_else(|| anyhow::anyhow!("No proof chain for skill '{skill}'"))?;
        let mut verification = chain.verify();

        // Fold in Ed25519 signature verification. Without this, a signed entry
        // whose combined_hash was altered (or whose signature is forged) still
        // passes the hash-chain consistency check. Fail closed: missing verify
        // material or any signature failure invalidates the chain.
        if let Some(key) = &self.verify_key {
            let report = chain.verify_signatures(key);
            if !report.is_ok() {
                verification.valid = false;
                for entry_id in report.failures {
                    verification.errors.push(format!(
                        "signature verification failed for entry {entry_id}"
                    ));
                }
            }
        } else {
            verification.valid = false;
            verification.errors.push(
                "SENTINEL_VERIFY_KEY is required for proof signature verification".to_string(),
            );
        }

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
    /// runs in step_judge (M1.4) upstream — so this only satisfies
    /// ProofEngine::new's signature.
    struct TestJudge;
    #[async_trait::async_trait]
    impl JudgeService for TestJudge {
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
        ProofEngine::new(state, Arc::new(TestJudge))
            .with_signing(None, false)
            .with_test_step_graph_authority()
    }

    fn engine_without_step_graph() -> ProofEngine {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        ProofEngine::new(state, Arc::new(TestJudge)).with_signing(None, false)
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[31u8; 32])
    }

    fn signed_engine() -> ProofEngine {
        let key = test_signing_key();
        let verifying = key.verifying_key();
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        ProofEngine::new(state, Arc::new(TestJudge))
            .with_signing(Some(key), true)
            .with_verify_key(Some(verifying))
            .with_test_step_graph_authority()
    }

    static LANGGRAPH_TENANT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_phase_graph_thread_env<R>(
        backend: Option<&str>,
        tenant: Option<&str>,
        f: impl FnOnce() -> R,
    ) -> R {
        let _guard = LANGGRAPH_TENANT_ENV_LOCK
            .lock()
            .expect("langgraph tenant env lock poisoned");
        let tenant_key = sentinel_domain::langgraph_thread::LANGGRAPH_TENANT_ENV;
        let backend_key = "SENTINEL_PHASE_GRAPH_CHECKPOINTER";
        let previous_tenant = std::env::var_os(tenant_key);
        let previous_backend = std::env::var_os(backend_key);
        match backend {
            Some(value) => std::env::set_var(backend_key, value),
            None => std::env::remove_var(backend_key),
        }
        match tenant {
            Some(value) => std::env::set_var(tenant_key, value),
            None => std::env::remove_var(tenant_key),
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        match previous_backend {
            Some(value) => std::env::set_var(backend_key, value),
            None => std::env::remove_var(backend_key),
        }
        match previous_tenant {
            Some(value) => std::env::set_var(tenant_key, value),
            None => std::env::remove_var(tenant_key),
        }

        match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn expected_phase_thread_id_ignores_tenant_for_local_sqlite_default() {
        with_phase_graph_thread_env(None, Some("legatus_ai"), || {
            let thread_id = ProofEngine::expected_phase_thread_id("linear", "session-123")
                .expect("valid local thread id");

            assert_eq!(thread_id, "sentinel.phase.linear.session-123");
        });
    }

    #[test]
    fn expected_phase_thread_id_uses_tenant_namespace_for_hosted_backend() {
        with_phase_graph_thread_env(Some("postgres"), Some("legatus_ai"), || {
            let thread_id = ProofEngine::expected_phase_thread_id("linear", "session-123")
                .expect("valid tenant thread id");

            assert_eq!(
                thread_id,
                "tenant:legatus_ai:sentinel.phase.linear.session-123"
            );
        });
    }

    #[test]
    fn expected_phase_thread_id_rejects_backend_aliases_without_normalization() {
        with_phase_graph_thread_env(Some("postgresql"), Some("legatus_ai"), || {
            let err = ProofEngine::expected_phase_thread_id("linear", "session-123")
                .expect_err("postgresql alias must be rejected");
            assert!(err
                .to_string()
                .contains("unsupported phase graph checkpointer backend 'postgresql'"));
            assert!(err
                .to_string()
                .contains("expected sqlite, postgres, or redis"));
        });

        with_phase_graph_thread_env(Some("redis-checkpoint"), Some("legatus_ai"), || {
            let err = ProofEngine::expected_phase_thread_id("linear", "session-123")
                .expect_err("redis-checkpoint alias must be rejected");
            assert!(err
                .to_string()
                .contains("unsupported phase graph checkpointer backend 'redis-checkpoint'"));
            assert!(err
                .to_string()
                .contains("expected sqlite, postgres, or redis"));
        });
    }

    #[test]
    fn expected_phase_thread_id_rejects_malformed_tenant_namespace_for_hosted_backend() {
        with_phase_graph_thread_env(Some("redis"), Some("tenant:escape"), || {
            let err = ProofEngine::expected_phase_thread_id("linear", "session-123")
                .expect_err("tenant delimiter injection must fail");
            assert!(err
                .to_string()
                .contains(sentinel_domain::langgraph_thread::LANGGRAPH_TENANT_ENV));
            assert!(err.to_string().contains("invalid characters"));
        });
    }

    fn hosted_topology(skill: &str, session_id: &str, phase_id: &str) -> serde_json::Value {
        let mut topology = test_graph_topology(skill, session_id, &[phase_id]);
        topology["thread_id"] = serde_json::json!(format!(
            "tenant:legatus_ai:sentinel.phase.{skill}.{session_id}"
        ));
        topology["checkpointer_backend"] = serde_json::json!("redis");
        topology["checkpointer_scope"] = serde_json::json!("ttl_seconds:none");
        topology["checkpointer_tenant_scope"] = serde_json::json!("legatus_ai");
        topology["nodes"][0]["metadata"]["sentinel.checkpointer_backend"] =
            serde_json::json!("redis");
        topology["nodes"][0]["metadata"]["sentinel.checkpointer_scope"] =
            serde_json::json!("ttl_seconds:none");
        topology["nodes"][0]["metadata"]["sentinel.checkpointer_tenant_scope"] =
            serde_json::json!("legatus_ai");
        topology
    }

    #[test]
    fn topology_validation_uses_serialized_tenant_scope_after_env_drift() {
        with_phase_graph_thread_env(Some("redis"), Some("wrong_tenant"), || {
            let graph = serde_json::json!({
                "graph_topology": hosted_topology("linear", "session-123", "claim")
            });
            let expected = ProofEngine::validate_graph_topology(
                graph.as_object().expect("graph object"),
                "linear",
                "session-123",
                "claim",
                &test_step_workflow("linear", "claim"),
                "test evidence",
            )
            .expect("hosted topology should validate from serialized tenant scope");

            assert_eq!(
                expected,
                "tenant:legatus_ai:sentinel.phase.linear.session-123"
            );
        });
    }

    #[test]
    fn topology_validation_rejects_hosted_backend_without_tenant_scope() {
        let mut topology = hosted_topology("linear", "session-123", "claim");
        topology
            .as_object_mut()
            .expect("topology object")
            .remove("checkpointer_tenant_scope");
        let graph = serde_json::json!({ "graph_topology": topology });

        let err = ProofEngine::validate_graph_topology(
            graph.as_object().expect("graph object"),
            "linear",
            "session-123",
            "claim",
            &test_step_workflow("linear", "claim"),
            "test evidence",
        )
        .expect_err("hosted topology without tenant scope must fail");

        assert!(err
            .to_string()
            .contains("omitted checkpointer tenant scope"));
    }

    #[test]
    fn topology_validation_rejects_mismatched_tenant_metadata() {
        let mut topology = hosted_topology("linear", "session-123", "claim");
        topology["nodes"][0]["metadata"]["sentinel.checkpointer_tenant_scope"] =
            serde_json::json!("other_tenant");
        let graph = serde_json::json!({ "graph_topology": topology });

        let err = ProofEngine::validate_graph_topology(
            graph.as_object().expect("graph object"),
            "linear",
            "session-123",
            "claim",
            &test_step_workflow("linear", "claim"),
            "test evidence",
        )
        .expect_err("mismatched tenant metadata must fail");

        assert!(err
            .to_string()
            .contains("matching checkpointer tenant scope metadata"));
    }

    #[test]
    fn topology_validation_rejects_sqlite_tenant_metadata() {
        let mut topology = test_graph_topology("linear", "session-123", &["claim"]);
        topology["checkpointer_tenant_scope"] = serde_json::json!("legatus_ai");
        let graph = serde_json::json!({ "graph_topology": topology });

        let err = ProofEngine::validate_graph_topology(
            graph.as_object().expect("graph object"),
            "linear",
            "session-123",
            "claim",
            &test_step_workflow("linear", "claim"),
            "test evidence",
        )
        .expect_err("sqlite topology with tenant scope must fail");

        assert!(err
            .to_string()
            .contains("must not carry hosted tenant metadata"));
    }

    #[test]
    fn topology_validation_rejects_phase_order_outside_workflow() {
        let topology = test_graph_topology("linear", "session-123", &["claim", "review"]);
        let graph = serde_json::json!({ "graph_topology": topology });

        let err = ProofEngine::validate_graph_topology(
            graph.as_object().expect("graph object"),
            "linear",
            "session-123",
            "claim",
            &test_step_workflow("linear", "claim"),
            "test evidence",
        )
        .expect_err("topology phase order must match configured workflow");

        assert!(err.to_string().contains("topology phase_order mismatch"));
    }

    struct StepGraphWithForgedWrite;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithForgedWrite {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let checkpoint_id = format!("forged-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state: workflow_state.clone(),
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": phase_id,
                        "value_json": {
                            "skill": skill,
                            "session_id": session_id,
                            "step_states": []
                        }
                    }],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithBoundaryWrite;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithBoundaryWrite {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let checkpoint_id = format!("boundary-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": "START",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "START",
                        "value_json": graph_state
                    }],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithPreviousCheckpointWrite;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithPreviousCheckpointWrite {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let latest_checkpoint_id = format!("latest-{session_id}-{phase_id}-{step_id}");
            let previous_checkpoint_id = format!("previous-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": latest_checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 2,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": latest_checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 2,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": previous_checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": phase_id,
                        "value_json": graph_state
                    }],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithMismatchedWriteThread;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithMismatchedWriteThread {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let checkpoint_id = format!("mismatched-thread-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.other-session",
                        "checkpoint_id": checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": phase_id,
                        "value_json": graph_state
                    }],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithOutOfOrderWriteHistory;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithOutOfOrderWriteHistory {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let latest_checkpoint_id =
                format!("out-of-order-latest-{session_id}-{phase_id}-{step_id}");
            let previous_checkpoint_id =
                format!("out-of-order-previous-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": latest_checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 2,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": latest_checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 2,
                        "state": graph_state.clone()
                    }],
                    "writes": [
                        {
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "checkpoint_id": latest_checkpoint_id,
                            "step_number": 2,
                            "channel": "state",
                            "node_id": phase_id,
                            "value_json": graph_state.clone()
                        },
                        {
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "checkpoint_id": previous_checkpoint_id,
                            "step_number": 1,
                            "channel": "state",
                            "node_id": phase_id,
                            "value_json": graph_state
                        }
                    ],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithOutOfOrderCheckpointHistory;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithOutOfOrderCheckpointHistory {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let latest_checkpoint_id =
                format!("out-of-order-checkpoint-latest-{session_id}-{phase_id}-{step_id}");
            let previous_checkpoint_id =
                format!("out-of-order-checkpoint-previous-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": latest_checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 2,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [
                        {
                            "checkpoint_id": latest_checkpoint_id,
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "step_number": 2,
                            "state": graph_state.clone()
                        },
                        {
                            "checkpoint_id": previous_checkpoint_id,
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "step_number": 1,
                            "state": graph_state.clone()
                        }
                    ],
                    "writes": [
                        {
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "checkpoint_id": previous_checkpoint_id,
                            "step_number": 1,
                            "channel": "state",
                            "node_id": phase_id,
                            "value_json": graph_state.clone()
                        },
                        {
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "checkpoint_id": latest_checkpoint_id,
                            "step_number": 2,
                            "channel": "state",
                            "node_id": phase_id,
                            "value_json": graph_state
                        }
                    ],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithStaleLatestCheckpoint;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithStaleLatestCheckpoint {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let stale_state = serde_json::json!({
                "skill": skill,
                "session_id": session_id,
                "step_states": []
            });
            let checkpoint_id = format!("stale-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": stale_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": stale_state
                    }],
                    "writes": [{
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": phase_id,
                        "value_json": graph_state
                    }],
                    "graph_topology": test_graph_topology(skill, session_id, &[phase_id])
                })),
            })
        }
    }

    struct StepGraphWithoutTopology;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithoutTopology {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let checkpoint_id = format!("no-topology-{session_id}-{phase_id}-{step_id}");
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": phase_id,
                        "value_json": graph_state
                    }]
                })),
            })
        }
    }

    struct StepGraphWithForgedTopology {
        mutate: fn(&mut serde_json::Value),
    }

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithForgedTopology {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            _step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let mut workflow_state = WorkflowState::new(skill, session_id);
            workflow_state.update_step(phase_id, step_id, status, summary);
            let graph_state = test_step_graph_state(&workflow_state, phase_id, _step_policy)?;
            let checkpoint_id = format!("forged-topology-{session_id}-{phase_id}-{step_id}");
            let mut graph_topology = test_graph_topology(skill, session_id, &[phase_id]);
            (self.mutate)(&mut graph_topology);
            Ok(StepGraphApplyResult {
                workflow_state,
                graph_run: test_graph_run_with_authority(serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": phase_id,
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": checkpoint_id,
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": checkpoint_id,
                        "step_number": 1,
                        "channel": "state",
                        "node_id": phase_id,
                        "value_json": graph_state
                    }],
                    "graph_topology": graph_topology
                })),
            })
        }
    }

    struct StepGraphWithoutWorkflowAuthority;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithoutWorkflowAuthority {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let authority = TestStepGraphAuthority::default();
            let mut result = authority
                .apply_step_status(
                    skill,
                    session_id,
                    workflow,
                    phase_id,
                    step_id,
                    step_policy,
                    status,
                    summary,
                )
                .await?;
            result
                .graph_run
                .as_object_mut()
                .expect("test graph run must be an object")
                .remove("workflow_authority");
            Ok(result)
        }
    }

    struct StepGraphWithoutStepPolicyEvidence;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithoutStepPolicyEvidence {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let authority = TestStepGraphAuthority::default();
            let mut result = authority
                .apply_step_status(
                    skill,
                    session_id,
                    workflow,
                    phase_id,
                    step_id,
                    step_policy,
                    status,
                    summary,
                )
                .await?;
            fn strip_step_policy_evidence(value: &mut serde_json::Value) {
                match value {
                    serde_json::Value::Object(obj) => {
                        obj.remove("step_policy_evidence");
                        for value in obj.values_mut() {
                            strip_step_policy_evidence(value);
                        }
                    }
                    serde_json::Value::Array(values) => {
                        for value in values {
                            strip_step_policy_evidence(value);
                        }
                    }
                    _ => {}
                }
            }

            strip_step_policy_evidence(&mut result.graph_run);
            Ok(result)
        }
    }

    struct StepGraphWithMismatchedProjection;

    #[async_trait::async_trait]
    impl StepGraphAuthority for StepGraphWithMismatchedProjection {
        async fn apply_step_status(
            &self,
            skill: &str,
            session_id: &str,
            workflow: &SkillWorkflow,
            phase_id: &str,
            step_id: &str,
            step_policy: &WorkflowStep,
            status: StepStatus,
            summary: Option<String>,
        ) -> Result<StepGraphApplyResult> {
            let authority = TestStepGraphAuthority::default();
            let mut result = authority
                .apply_step_status(
                    skill,
                    session_id,
                    workflow,
                    phase_id,
                    step_id,
                    step_policy,
                    status,
                    summary,
                )
                .await?;
            result.workflow_state.session_id = "other-session".to_string();
            Ok(result)
        }
    }

    /// A judge whose `evaluate_multi` returns a fixed two-judge verdict (Opus +
    /// Codex), so the dual fold in `judge_verdict_for` can be exercised without
    /// the network. `evaluate` (single path) is unreachable here.
    struct DualTestJudge {
        opus_sufficient: bool,
        codex_sufficient: bool,
    }

    #[async_trait::async_trait]
    impl JudgeService for DualTestJudge {
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
            Arc::new(DualTestJudge {
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
        assert!(
            !v.sufficient,
            "a single dissent must fail the completion verdict"
        );
    }

    #[tokio::test]
    async fn passing_verdict_seals_step_proof_into_chain() {
        let eng = signed_engine();
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
        let chain = state.proof_chain("linear").expect("chain exists");
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
    async fn step_submission_without_graph_authority_fails_before_chain_mutation() {
        let eng = engine_without_step_graph();

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("missing step graph authority must fail closed");

        assert!(
            err.to_string().contains("LangGraph step authority"),
            "error must name the missing authority: {err:#}"
        );
        let state = eng.state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "missing step graph authority must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "missing step graph authority must not mutate workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_rejects_graph_run_without_workflow_authority() {
        let state = Arc::new(RwLock::new(SessionState::new("missing-authority-step")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithoutWorkflowAuthority));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("step graph evidence without authority must fail closed");

        assert!(
            err.to_string().contains("omitted workflow_authority"),
            "error must identify missing graph authority marker: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "unauthorized graph evidence must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "unauthorized graph evidence must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_rejects_graph_run_without_step_policy_evidence() {
        let state = Arc::new(RwLock::new(SessionState::new(
            "missing-step-policy-evidence",
        )));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithoutStepPolicyEvidence));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("step graph evidence without policy evidence must fail closed");

        assert!(
            err.to_string()
                .contains("omitted state.step_policy_evidence"),
            "error must identify missing configured policy evidence: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "policyless graph evidence must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "policyless graph evidence must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_rejects_workflow_projection_session_mismatch() {
        let state = Arc::new(RwLock::new(SessionState::new("projection-step-session")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithMismatchedProjection));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("workflow projection identity mismatch must fail closed");

        assert!(
            err.to_string()
                .contains("workflow projection session mismatch"),
            "error must identify forged projection session: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "forged workflow projection must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "forged workflow projection must not mutate workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_requires_accepted_graph_state_in_write_history() {
        let state = Arc::new(RwLock::new(SessionState::new("forged-step-session")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithForgedWrite));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("forged write history must fail closed");

        assert!(
            err.to_string().contains("accepted graph state"),
            "error must identify missing accepted graph-state write: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "invalid graph write evidence must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "invalid graph write evidence must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_rejects_boundary_attributed_graph_write() {
        let state = Arc::new(RwLock::new(SessionState::new("boundary-step-session")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithBoundaryWrite));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("boundary-attributed graph writes must fail closed");

        assert!(
            err.to_string().contains("expected phase node 'claim'"),
            "error must identify boundary-attributed graph write: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "boundary-attributed graph write must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "boundary-attributed graph write must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_requires_write_history_for_latest_checkpoint() {
        let state = Arc::new(RwLock::new(SessionState::new("spliced-step-session")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithPreviousCheckpointWrite));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("write history spliced from an older checkpoint must fail closed");

        assert!(
            err.to_string()
                .contains("latest-checkpoint state-channel write"),
            "error must identify missing latest-checkpoint write evidence: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "spliced graph write evidence must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "spliced graph write evidence must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_requires_write_history_for_matching_thread() {
        let state = Arc::new(RwLock::new(SessionState::new("mismatched-step-thread")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithMismatchedWriteThread));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("write history from another thread must fail closed");

        assert!(
            err.to_string().contains("write history contains thread"),
            "error must identify mismatched write thread: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "mismatched graph write thread must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "mismatched graph write thread must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_requires_oldest_first_write_history() {
        let state = Arc::new(RwLock::new(SessionState::new("out-of-order-step-writes")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithOutOfOrderWriteHistory));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("out-of-order write history must fail closed");

        assert!(
            err.to_string()
                .contains("write history is not oldest-first"),
            "error must identify out-of-order write history: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "out-of-order graph write history must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "out-of-order graph write history must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_requires_oldest_first_checkpoint_history() {
        let state = Arc::new(RwLock::new(SessionState::new(
            "out-of-order-step-checkpoints",
        )));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithOutOfOrderCheckpointHistory));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("out-of-order checkpoint history must fail closed");

        assert!(
            err.to_string()
                .contains("checkpoint history is not oldest-first"),
            "error must identify out-of-order checkpoint history: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "out-of-order graph checkpoint history must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "out-of-order graph checkpoint history must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_rejects_stale_latest_checkpoint_state() {
        let state = Arc::new(RwLock::new(SessionState::new("stale-step-session")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithStaleLatestCheckpoint));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("stale latest checkpoint must fail closed");

        assert!(
            err.to_string().contains("latest checkpoint state mismatch"),
            "error must identify stale latest checkpoint state: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "stale graph checkpoint evidence must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "stale graph checkpoint evidence must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_requires_compiled_graph_topology() {
        let state = Arc::new(RwLock::new(SessionState::new("no-topology-step-session")));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithoutTopology));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("missing graph topology must fail closed");

        assert!(
            err.to_string().contains("omitted compiled graph topology"),
            "error must identify missing compiled graph topology: {err:#}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "missing topology evidence must not seal a StepProof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "missing topology evidence must not project workflow progress"
        );
    }

    #[tokio::test]
    async fn step_submission_rejects_topology_without_langgraph_edges() {
        fn remove_edges(topology: &mut serde_json::Value) {
            topology["edges"] = serde_json::json!([]);
        }

        let state = Arc::new(RwLock::new(SessionState::new(
            "bad-topology-edge-step-session",
        )));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithForgedTopology {
                mutate: remove_edges,
            }));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("empty topology edges must fail closed");

        assert!(
            err.to_string().contains("topology had no edges"),
            "error must identify missing LangGraph edges: {err:#}"
        );
        let state = state.read().await;
        assert!(state.proof_chain("linear").is_none());
        assert!(!state.has_graph_workflow("linear"));
    }

    #[tokio::test]
    async fn step_submission_rejects_topology_without_node_timeout_policy() {
        fn remove_timeout(topology: &mut serde_json::Value) {
            topology["nodes"][0]["has_timeout_policy"] = serde_json::json!(false);
        }

        let state = Arc::new(RwLock::new(SessionState::new(
            "bad-topology-node-step-session",
        )));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithForgedTopology {
                mutate: remove_timeout,
            }));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("missing timeout metadata must fail closed");

        assert!(
            err.to_string().contains("omitted timeout policy"),
            "error must identify missing timeout policy: {err:#}"
        );
        let state = state.read().await;
        assert!(state.proof_chain("linear").is_none());
        assert!(!state.has_graph_workflow("linear"));
    }

    #[tokio::test]
    async fn step_submission_rejects_topology_without_auto_checkpointing() {
        fn disable_auto_checkpoint(topology: &mut serde_json::Value) {
            topology["auto_checkpoint"] = serde_json::json!(false);
        }

        let state = Arc::new(RwLock::new(SessionState::new(
            "bad-topology-runtime-step-session",
        )));
        let eng = ProofEngine::new(state.clone(), Arc::new(TestJudge))
            .with_signing(None, false)
            .with_step_graph_authority(Arc::new(StepGraphWithForgedTopology {
                mutate: disable_auto_checkpoint,
            }));

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("disabled auto-checkpointing must fail closed");

        assert!(
            err.to_string().contains("auto-checkpointing"),
            "error must identify missing auto-checkpointing: {err:#}"
        );
        let state = state.read().await;
        assert!(state.proof_chain("linear").is_none());
        assert!(!state.has_graph_workflow("linear"));
    }

    #[tokio::test]
    async fn duplicate_step_submission_rejected_before_second_step_proof() {
        let eng = signed_engine();

        eng.submit_step_evidence(
            "linear",
            "claim",
            "1",
            "fetch ticket",
            Evidence::default(),
            JudgeVerdict::pass(0.93, "evidence sufficient"),
            JudgeModel::Sonnet,
            serde_json::Value::Null,
            None,
            Utc::now(),
        )
        .await
        .expect("initial step proof");

        let err = eng
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket again",
                Evidence::default(),
                JudgeVerdict::pass(0.93, "evidence sufficient"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .expect_err("duplicate step must be rejected by graph authority");

        assert!(
            err.to_string().contains("already terminal"),
            "duplicate step error must come from graph terminal-step authority: {err:#}"
        );
        let state = eng.state.read().await;
        let chain = state.proof_chain("linear").expect("chain");
        assert_eq!(
            chain.entries.len(),
            1,
            "duplicate step submit must not seal a second StepProof"
        );
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
            state.proof_chain("linear").is_none(),
            "refused seal must not mutate the chain"
        );
    }

    #[tokio::test]
    async fn default_signing_posture_refuses_unsigned_step_seal() {
        let state = Arc::new(RwLock::new(SessionState::new("default-signing-session")));
        let eng =
            ProofEngine::new(state.clone(), Arc::new(TestJudge)).with_test_step_graph_authority();

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

        assert!(
            result.is_err(),
            "ProofEngine::new must default to mandatory signing"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("SENTINEL_SIGNING_KEY") && msg.contains("unsigned StepProof"),
            "error must explain the default missing-key refusal: {msg}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chain("linear").is_none(),
            "default signing refusal must not mutate the chain"
        );
    }

    #[tokio::test]
    async fn local_unsigned_seal_is_not_authoritative_verification_material() {
        let eng = engine(); // local construction: no key, required posture disabled
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
            .expect("local unsigned seal can be constructed");
        assert!(
            proof.signature.is_none(),
            "local unsigned material has no authority signature"
        );
    }

    #[tokio::test]
    async fn verification_without_verify_key_fails_chain_verification() {
        let key = SigningKey::from_bytes(&[13u8; 32]);
        let eng = engine().with_signing(Some(key), true);
        eng.submit_step_evidence(
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
        .expect("signing key allows sealing");

        let verification = eng
            .verify_chain("linear")
            .await
            .expect("verification report");
        assert!(
            !verification.valid,
            "verification must fail without SENTINEL_VERIFY_KEY"
        );
        assert!(
            verification
                .errors
                .iter()
                .any(|error| error.contains("SENTINEL_VERIFY_KEY")),
            "verification errors must explain the missing verify key: {:?}",
            verification.errors
        );
    }

    #[tokio::test]
    async fn insufficient_verdict_hard_fails_without_mutating_chain() {
        let eng = signed_engine();
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
            !state.has_proof_chain("linear"),
            "no chain should be created when verdict fails",
        );
    }

    #[tokio::test]
    async fn sequential_step_proofs_chain_via_head_hash() {
        let eng = signed_engine();

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
        // proof (`proofs` Vec), then drops into step-level work (mixed
        // `entries` Vec). The step's previous_hash must point at
        // the phase's combined_hash via head_hash().
        let eng = signed_engine();

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
            let mut phase_proof = PhaseProof {
                phase_id: "claim".into(),
                skill: "linear".into(),
                session_id: "test-session".into(),
                evidence,
                evidence_hash,
                previous_hash: sentinel_domain::proof::GENESIS_HASH.into(),
                combined_hash: combined_hash.clone(),
                judge_model: "sonnet".into(),
                judge_verdict: JudgeVerdict::pass(0.95, "claimed"),
                signature: None,
                started_at: Utc::now() - chrono::Duration::seconds(10),
                completed_at: Utc::now() - chrono::Duration::seconds(5),
                duration_ms: 5000,
            };
            phase_proof.sign_with(&test_signing_key());
            chain.add_proof(phase_proof).expect("seed phase");
            state.restore_proof_chain("linear", chain);
        }

        // Now submit a step. Its previous_hash should match the phase's
        // combined_hash because phase proofs live in the canonical entries
        // chain.
        let phase_combined = {
            let state = eng.state.read().await;
            state.proof_chain("linear").unwrap().head_hash().to_string()
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

#[cfg(test)]
mod phase_evidence_tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::judge_service::JudgeService;
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
    use sentinel_domain::workflow::{SkillWorkflow, StepStatus, WorkflowPhase, WorkflowState};

    struct PhaseTestJudge {
        verdict: JudgeVerdict,
    }

    #[async_trait::async_trait]
    impl JudgeService for PhaseTestJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            Ok(self.verdict.clone())
        }
    }

    #[derive(Default)]
    struct RecordingPhaseGraph {
        calls: Mutex<Vec<(String, bool)>>,
        fail_with: Mutex<Option<String>>,
        seed_state: Mutex<Option<WorkflowState>>,
        graph_run_override: Mutex<Option<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl PhaseGraphAuthority for RecordingPhaseGraph {
        async fn apply_verdict(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            passed: bool,
        ) -> Result<PhaseGraphApplyResult> {
            self.calls
                .lock()
                .unwrap()
                .push((phase_id.to_string(), passed));
            if let Some(message) = self.fail_with.lock().unwrap().take() {
                bail!("{message}");
            }
            let mut state = self
                .seed_state
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| WorkflowState::new(skill, session_id));
            state.current_phase = Some(if passed { 1 } else { 0 });
            if passed && !state.completed_phases.iter().any(|p| p == phase_id) {
                state.completed_phases.push(phase_id.to_string());
            }
            state.complete = passed;
            let graph_run = self
                .graph_run_override
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| {
                    let graph_state = serde_json::json!({
                        "skill": skill,
                        "session_id": session_id,
                        "phase_order": ["claim"],
                        "current_phase": state.current_phase,
                        "completed_phases": state.completed_phases.clone(),
                        "complete": state.complete,
                        "dyad_verdicts": state.dyad_verdicts.clone(),
                        "step_states": state.step_states.clone(),
                        "current_step": state.current_step.clone(),
                        "last_verdict": if passed { "pass" } else { "fail" },
                    });
                    let checkpoint_id = format!("checkpoint-{phase_id}");
                    test_graph_run_with_authority(serde_json::json!({
                        "state": graph_state.clone(),
                        "latest_checkpoint": {
                            "checkpoint_id": checkpoint_id,
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "step_number": 1,
                            "state": graph_state.clone(),
                            "writes": [{
                                "node_id": phase_id,
                                "channel": "state",
                                "ts": "2026-06-17T00:00:00Z",
                            }],
                        },
                        "checkpoints": [{
                            "checkpoint_id": checkpoint_id,
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "step_number": 1,
                            "state": graph_state.clone(),
                        }],
                        "writes": [{
                            "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                            "checkpoint_id": checkpoint_id,
                            "step_number": 1,
                            "channel": "state",
                            "node_id": phase_id,
                            "value_json": graph_state.clone(),
                        }],
                        "graph_topology": test_graph_topology(skill, session_id, &[phase_id]),
                        "stream": if state.complete {
                            serde_json::json!([])
                        } else {
                            serde_json::json!([
                                {
                                    "event_type": "ExecutionComplete",
                                    "node_id": phase_id,
                                    "timestamp": "2026-06-17T00:00:00Z",
                                    "superstep": 1,
                                    "payload_kind": "values",
                                    "payload_json": {
                                        "skill": skill,
                                        "session_id": session_id
                                    }
                                },
                                {
                                    "event_type": "Checkpoint",
                                    "node_id": phase_id,
                                    "timestamp": "2026-06-17T00:00:00Z",
                                    "superstep": 1,
                                    "payload_kind": "checkpoints",
                                    "payload_json": {
                                        "checkpoint_id": checkpoint_id,
                                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                                        "step_number": 1,
                                        "source": {
                                            "type": "stream_update",
                                            "node": phase_id
                                        },
                                        "state": graph_state.clone()
                                    }
                                },
                                {
                                    "event_type": "Custom",
                                    "node_id": phase_id,
                                    "timestamp": "2026-06-17T00:00:00Z",
                                    "superstep": 1,
                                    "payload_kind": "custom",
                                    "payload_json": {
                                        "type": "sentinel.phase_gate",
                                        "skill": skill,
                                        "session_id": session_id,
                                        "phase_id": phase_id,
                                        "phase_index": state.current_phase,
                                        "last_verdict": "pending"
                                    }
                                }
                        ])
                        }
                    }))
                });
            Ok(PhaseGraphApplyResult {
                workflow_state: state,
                graph_run,
            })
        }
    }

    fn workflow() -> SkillWorkflow {
        SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![WorkflowPhase {
                id: "claim".to_string(),
                file: "claim.md".to_string(),
                required: true,
                judge: JudgeModel::Sonnet,
                description: "claim phase".to_string(),
                required_dyad: None,
            }],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    #[tokio::test]
    async fn phase_submission_projects_langgraph_authority_state() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph::default());
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();
        let mut graph_projection = WorkflowState::new("linear", "phase-session");
        graph_projection.update_step(
            "claim",
            "0.1",
            StepStatus::Completed,
            Some("step done".into()),
        );
        *authority.seed_state.lock().unwrap() = Some(graph_projection);

        engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect("phase proof seals and graph advances");

        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)]
        );
        let state = state.read().await;
        let workflow_state = state
            .graph_workflow("linear")
            .expect("workflow projected from graph");
        assert_eq!(workflow_state.completed_phases, vec!["claim".to_string()]);
        assert!(workflow_state.complete);
        assert_eq!(workflow_state.step_states.len(), 1);
        assert_eq!(workflow_state.step_states[0].step_id, "0.1");
        assert_eq!(workflow_state.step_states[0].status, StepStatus::Completed);
        assert_eq!(
            state
                .proof_chain("linear")
                .expect("proof chain")
                .phase_count(),
            1
        );
    }

    #[tokio::test]
    async fn phase_submission_rejects_workflow_projection_session_mismatch() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph::default());
        *authority.seed_state.lock().unwrap() = Some(WorkflowState::new("linear", "other-session"));
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("workflow projection identity mismatch must fail closed");

        assert!(
            err.to_string()
                .contains("workflow projection session mismatch"),
            "error must identify forged projection session: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then projection validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "forged workflow projection must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "forged workflow projection must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_with_signing_key_signs_phase_proof() {
        let state = Arc::new(RwLock::new(SessionState::new("signed-phase-session")));
        let authority = Arc::new(RecordingPhaseGraph::default());
        let key = SigningKey::from_bytes(&[41u8; 32]);
        let verifying = key.verifying_key();
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(Some(key), true)
        .with_phase_graph_authority(authority);
        let wf = workflow();

        let proof = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect("signed phase proof seals");

        assert!(
            proof.signature.is_some(),
            "configured signing key must sign PhaseProof"
        );
        assert!(
            proof
                .verify_signature(&verifying)
                .expect("phase signature verifies"),
            "phase signature must verify against configured public key"
        );
    }

    #[tokio::test]
    async fn failed_phase_submission_persists_fail_verdict_through_graph() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph::default());
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec!["add log".into()]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("insufficient phase proof must fail");

        assert!(
            err.to_string().contains("evidence insufficient"),
            "error must still surface insufficient evidence: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), false)],
            "failed verdict must be committed through LangGraph authority"
        );
        let state = state.read().await;
        let workflow_state = state
            .graph_workflow("linear")
            .expect("failed graph verdict still projects workflow state");
        assert_eq!(workflow_state.current_phase, Some(0));
        assert!(workflow_state.completed_phases.is_empty());
        assert!(!workflow_state.complete);
        assert_eq!(
            state
                .submission_attempts("linear:claim")
                .map(|attempts| attempts.count),
            Some(1),
            "failure counter is recorded after graph accepts the fail verdict"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "failed verdict must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_missing_custom_gate_stream_fails_closed() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": {
                        "skill": "linear",
                        "session_id": "phase-session",
                        "phase_order": ["claim"],
                        "current_phase": 0,
                        "completed_phases": [],
                        "complete": false
                    },
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "phase_order": ["claim"],
                            "current_phase": 0,
                            "completed_phases": [],
                            "complete": false
                        },
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "phase_order": ["claim"],
                            "current_phase": 0,
                            "completed_phases": [],
                            "complete": false
                        }
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "phase_order": ["claim"],
                            "current_phase": 0,
                            "completed_phases": [],
                            "complete": false
                        }
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": [
                        {
                            "event_type": "ExecutionComplete",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "values",
                            "payload_json": {
                                "skill": "linear",
                                "session_id": "phase-session"
                            }
                        },
                        {
                            "event_type": "Checkpoint",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "checkpoints",
                            "payload_json": {
                                "checkpoint_id": "checkpoint-claim",
                                "thread_id": "sentinel.phase.linear.phase-session",
                                "step_number": 1,
                                "source": {
                                    "type": "stream_update",
                                    "node": "claim"
                                },
                                "state": {
                                    "skill": "linear",
                                    "session_id": "phase-session",
                                    "phase_order": ["claim"],
                                    "current_phase": 0,
                                    "completed_phases": [],
                                    "complete": false
                                }
                            }
                        }
                    ]
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec![]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("missing custom stream evidence must fail closed");

        assert!(
            err.to_string().contains("omitted custom phase-gate"),
            "error must identify missing custom gate stream: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), false)],
            "graph authority should be called, then evidence validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "invalid graph evidence must not project workflow state"
        );
        assert!(
            state.submission_attempts("linear:claim").is_none(),
            "local failure counters must not advance when graph evidence is invalid"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "invalid graph evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_rejects_mismatched_graph_state_authority() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let mut graph_run = test_nonterminal_phase_graph_run(
            "linear",
            "phase-session",
            "claim",
            Some(serde_json::json!({
                "type": "sentinel.phase_gate",
                "skill": "linear",
                "session_id": "phase-session",
                "phase_id": "claim",
                "phase_index": 0,
                "last_verdict": "pending"
            })),
        );
        graph_run
            .as_object_mut()
            .expect("test graph run must be an object")
            .insert(
                "graph_state".to_string(),
                serde_json::json!({
                    "skill": "linear",
                    "session_id": "phase-session",
                    "phase_order": ["claim"],
                    "current_phase": 1,
                    "completed_phases": ["claim"],
                    "complete": true,
                    "last_verdict": "pass"
                }),
            );
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(graph_run)),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec![]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("mismatched graph_state authority must fail closed");

        assert!(
            err.to_string().contains("graph_state mismatch"),
            "error must identify forged graph_state authority: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), false)],
            "graph authority should be called, then authority-field validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "mismatched graph_state must not project workflow state"
        );
        assert!(
            state.submission_attempts("linear:claim").is_none(),
            "mismatched graph_state must not advance local failure counters"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "mismatched graph_state must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_rejects_cross_session_custom_gate_stream() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_nonterminal_phase_graph_run(
                "linear",
                "phase-session",
                "claim",
                Some(serde_json::json!({
                    "type": "sentinel.phase_gate",
                    "skill": "linear",
                    "session_id": "other-session",
                    "phase_id": "claim",
                    "phase_index": 0,
                    "last_verdict": "pending"
                })),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec![]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("cross-session custom stream evidence must fail closed");

        assert!(
            err.to_string().contains("omitted custom phase-gate"),
            "error must identify mismatched custom gate stream: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), false)],
            "graph authority should be called, then custom stream identity validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "cross-session custom stream evidence must not project workflow state"
        );
        assert!(
            state.submission_attempts("linear:claim").is_none(),
            "local failure counters must not advance when custom stream evidence is invalid"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "cross-session custom stream evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_latest_checkpoint_stream_payload() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let graph_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 0,
            "completed_phases": [],
            "complete": false,
            "last_verdict": "fail"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": graph_state.clone()
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": [
                        {
                            "event_type": "ExecutionComplete",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "values",
                            "payload_json": graph_state.clone()
                        },
                        {
                            "event_type": "Checkpoint",
                            "node_id": "other_gate",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "checkpoints",
                            "payload_json": {
                                "checkpoint_id": "previous-checkpoint-claim",
                                "thread_id": "sentinel.phase.linear.phase-session",
                                "step_number": 1,
                                "source": {
                                    "type": "stream_update",
                                    "node": "other_gate"
                                },
                                "state": graph_state.clone()
                            }
                        },
                        {
                            "event_type": "Custom",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "custom",
                            "payload_json": {
                                "type": "sentinel.phase_gate",
                                "skill": "linear",
                                "session_id": "phase-session",
                                "phase_id": "claim",
                                "phase_index": 0,
                                "last_verdict": "pending"
                            }
                        }
                    ]
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec![]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("stream from a different checkpoint must fail closed");

        assert!(
            err.to_string()
                .contains("stream omitted checkpoint payload for gate 'claim'"),
            "error must identify missing gate stream checkpoint: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), false)],
            "graph authority should be called, then stream checkpoint validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "invalid stream checkpoint evidence must not project workflow state"
        );
        assert!(
            state.submission_attempts("linear:claim").is_none(),
            "local failure counters must not advance when stream evidence is invalid"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "invalid stream checkpoint evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_stream_checkpoint_state_match() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let graph_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 0,
            "completed_phases": [],
            "complete": false,
            "last_verdict": "fail"
        });
        let forged_stream_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 0,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": graph_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": graph_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": graph_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": graph_state.clone()
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": [
                        {
                            "event_type": "ExecutionComplete",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "values",
                            "payload_json": graph_state.clone()
                        },
                        {
                            "event_type": "Checkpoint",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "checkpoints",
                            "payload_json": {
                                "checkpoint_id": "checkpoint-claim",
                                "thread_id": "sentinel.phase.linear.phase-session",
                                "step_number": 1,
                                "source": {
                                    "type": "stream_update",
                                    "node": "claim"
                                },
                                "state": forged_stream_state
                            }
                        },
                        {
                            "event_type": "Custom",
                            "node_id": "claim",
                            "timestamp": "2026-06-17T00:00:00Z",
                            "superstep": 1,
                            "payload_kind": "custom",
                            "payload_json": {
                                "type": "sentinel.phase_gate",
                                "skill": "linear",
                                "session_id": "phase-session",
                                "phase_id": "claim",
                                "phase_index": 0,
                                "last_verdict": "pending"
                            }
                        }
                    ]
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec![]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("forged stream checkpoint state must fail closed");

        assert!(
            err.to_string()
                .contains("workflow projection completed_phases mismatch"),
            "error must identify forged stream checkpoint state: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), false)],
            "graph authority should be called, then stream state validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "forged stream checkpoint state must not project workflow state"
        );
        assert!(
            state.submission_attempts("linear:claim").is_none(),
            "local failure counters must not advance when stream state is invalid"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "forged stream checkpoint state must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_rejects_stale_latest_checkpoint_state() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": {
                        "skill": "linear",
                        "session_id": "phase-session",
                        "phase_order": ["claim"],
                        "current_phase": 1,
                        "completed_phases": ["claim"],
                        "complete": true
                    },
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "phase_order": ["claim"],
                            "current_phase": 0,
                            "completed_phases": [],
                            "complete": false
                        }
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "phase_order": ["claim"],
                            "current_phase": 0,
                            "completed_phases": [],
                            "complete": false
                        }
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "completed_phases": ["claim"]
                        }
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("stale latest checkpoint must fail closed");

        assert!(
            err.to_string().contains("latest checkpoint state mismatch"),
            "error must identify stale latest checkpoint state: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then evidence validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "stale graph checkpoint evidence must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "stale graph checkpoint evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_accepted_graph_state_in_write_history() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": {
                            "skill": "linear",
                            "session_id": "phase-session",
                            "completed_phases": ["claim"]
                        }
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("forged phase write history must fail closed");

        assert!(
            err.to_string().contains("accepted graph state"),
            "error must identify missing accepted graph-state write: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then write evidence validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "invalid graph write evidence must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "invalid graph write evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_rejects_write_value_field_without_value_json() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "value": accepted_state
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("write history must require value_json");

        assert!(
            err.to_string().contains("accepted graph state"),
            "error must reject write history without value_json: {err:#}"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "value-only write evidence must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "value-only write evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_write_history_for_latest_checkpoint() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 2,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 2,
                        "state": accepted_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "previous-checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": accepted_state
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("write history spliced from an older checkpoint must fail closed");

        assert!(
            err.to_string()
                .contains("latest-checkpoint state-channel write"),
            "error must identify missing latest-checkpoint write evidence: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then write evidence validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "spliced graph write evidence must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "spliced graph write evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_write_history_for_matching_thread() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.other-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": accepted_state
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("write history from another thread must fail closed");

        assert!(
            err.to_string().contains("write history contains thread"),
            "error must identify mismatched write thread: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then write thread validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "mismatched graph write thread must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "mismatched graph write thread must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_checkpoint_history_for_matching_thread() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.other-session",
                        "step_number": 1,
                        "state": accepted_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": accepted_state
                    }],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("checkpoint history from another thread must fail closed");

        assert!(
            err.to_string()
                .contains("checkpoint history contains thread"),
            "error must identify mismatched checkpoint history thread: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then checkpoint thread validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "mismatched graph checkpoint thread must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "mismatched graph checkpoint thread must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_oldest_first_write_history() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 2,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 2,
                        "state": accepted_state.clone()
                    }],
                    "writes": [
                        {
                            "thread_id": "sentinel.phase.linear.phase-session",
                            "checkpoint_id": "checkpoint-claim",
                            "step_number": 2,
                            "channel": "state",
                            "node_id": "claim",
                            "value_json": accepted_state.clone()
                        },
                        {
                            "thread_id": "sentinel.phase.linear.phase-session",
                            "checkpoint_id": "previous-checkpoint-claim",
                            "step_number": 1,
                            "channel": "state",
                            "node_id": "claim",
                            "value_json": accepted_state
                        }
                    ],
                    "graph_topology": test_graph_topology("linear", "phase-session", &["claim"]),
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("out-of-order write history must fail closed");

        assert!(
            err.to_string()
                .contains("write history is not oldest-first"),
            "error must identify out-of-order write history: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then write order validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "out-of-order graph write history must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "out-of-order graph write history must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_requires_compiled_graph_topology() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let accepted_state = serde_json::json!({
            "skill": "linear",
            "session_id": "phase-session",
            "phase_order": ["claim"],
            "current_phase": 1,
            "completed_phases": ["claim"],
            "complete": true,
            "last_verdict": "pass"
        });
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": accepted_state.clone(),
                    "latest_checkpoint": {
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone(),
                        "writes": [{
                            "node_id": "claim",
                            "channel": "state",
                            "ts": "2026-06-17T00:00:00Z"
                        }]
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-claim",
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "step_number": 1,
                        "state": accepted_state.clone()
                    }],
                    "writes": [{
                        "thread_id": "sentinel.phase.linear.phase-session",
                        "checkpoint_id": "checkpoint-claim",
                        "step_number": 1,
                        "channel": "state",
                        "node_id": "claim",
                        "value_json": accepted_state
                    }],
                    "stream": []
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("missing graph topology must fail closed");

        assert!(
            err.to_string().contains("omitted compiled graph topology"),
            "error must identify missing compiled graph topology: {err:#}"
        );
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[("claim".to_string(), true)],
            "graph authority should be called, then topology validation should fail"
        );
        let state = state.read().await;
        assert!(
            !state.has_graph_workflow("linear"),
            "missing topology evidence must not project workflow state"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "missing topology evidence must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_without_workflow_context_never_falls_back() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph::default());
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::fail(0.72, "claim needs evidence", vec![]),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                None,
                false,
            )
            .await
            .expect_err("workflow context is required for graph authority");

        assert!(
            err.to_string().contains("workflow context"),
            "error must identify missing graph workflow context: {err:#}"
        );
        assert!(
            authority.calls.lock().unwrap().is_empty(),
            "missing workflow context must not call graph authority with a guessed workflow"
        );
        let state = state.read().await;
        assert!(
            state.submission_attempts("linear:claim").is_none(),
            "missing workflow context must not use local failure counters"
        );
        assert!(
            !state.has_proof_chain("linear"),
            "missing workflow context must not seal a phase proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_graph_rejection_does_not_seal_proof() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph {
            fail_with: Mutex::new(Some("graph rejected phase order".to_string())),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority.clone());
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("graph rejection must fail submission");

        assert!(
            err.to_string().contains("graph rejected phase order"),
            "graph authority error must surface: {err:#}"
        );
        let state = state.read().await;
        assert!(
            !state.has_proof_chain("linear"),
            "graph-rejected phase must not seal a success proof"
        );
    }

    #[tokio::test]
    async fn phase_submission_malformed_graph_evidence_does_not_project_or_seal() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let authority = Arc::new(RecordingPhaseGraph {
            graph_run_override: Mutex::new(Some(test_graph_run_with_authority(
                serde_json::json!({
                    "state": {
                        "skill": "linear",
                        "session_id": "phase-session",
                        "current_phase": 1,
                        "completed_phases": ["claim"],
                        "complete": true
                    }
                }),
            ))),
            ..Default::default()
        });
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        )
        .with_signing(None, false)
        .with_phase_graph_authority(authority);
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("malformed graph evidence must fail closed");

        assert!(
            err.to_string().contains("omitted latest checkpoint"),
            "error must identify malformed graph evidence: {err:#}"
        );
        let state = state.read().await;
        assert!(
            !state.has_proof_chain("linear"),
            "malformed graph evidence must not seal a phase proof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "malformed graph evidence must not project workflow state"
        );
    }

    #[tokio::test]
    async fn phase_submission_without_graph_authority_fails_before_chain_mutation() {
        let state = Arc::new(RwLock::new(SessionState::new("phase-session")));
        let engine = ProofEngine::new(
            state.clone(),
            Arc::new(PhaseTestJudge {
                verdict: JudgeVerdict::pass(0.95, "done"),
            }),
        );
        let wf = workflow();

        let err = engine
            .submit_evidence(
                "linear",
                "claim",
                "claim phase",
                Evidence::default(),
                JudgeModel::Sonnet,
                Utc::now(),
                Some(&wf),
                false,
            )
            .await
            .expect_err("missing graph authority must fail closed");

        assert!(
            err.to_string().contains("LangGraph phase authority"),
            "error must name the missing authority: {err:#}"
        );
        let state = state.read().await;
        assert!(
            !state.has_proof_chain("linear"),
            "missing graph authority must not seal a phase proof"
        );
        assert!(
            !state.has_graph_workflow("linear"),
            "missing graph authority must not mutate workflow progress"
        );
    }
}
