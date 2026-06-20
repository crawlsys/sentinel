//! Tier B enforcement escalation graph — the LangGraph-Rust decision spine.
//!
//! Strict mode: an un-ready *started* ticket is reverted immediately — but
//! never on a single raw signal. The decision runs through a checkpointed
//! `langgraph-core` `StateGraph` so every revert carries a durable, replayable
//! audit trail (which fields were missing, what the adversarial judge said,
//! why we acted). The graph is the DECISION; the I/O (Linear fetch/revert,
//! the LLM judge) happens in the async orchestrator; the decision graph itself
//! runs through LangGraph-Rust's async node path with checkpointed thread IDs.
//!
//! Flow:
//! ```text
//!   classify ──(dev-ready or not started)──▶ clear ─▶ END
//!       │
//!       └──(started + not dev-ready)──▶ judge ──(confirmed)──▶ revert ─▶ END
//!                                          │
//!                                          └──(refuted: transient/label-lag)──▶ clear ─▶ END
//! ```
//! The `judge` step is an adversarial `Codex` pass (via `OpenRouter`) that must
//! CONFIRM the ticket is genuinely un-ready before a strict revert fires —
//! this guards against reverting on a Linear API race or label-propagation lag.

use langgraph_core::application::services::GraphCompiler;
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::delegation_service::{delegate, DelegationRequest, Worker};
use sentinel_domain::ports::LlmPort;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};
use crate::linear_enforcer::TicketReadiness;

/// The terminal decision the graph reaches for one ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Decision {
    /// No action — the ticket is dev-ready, not started, or the judge refuted.
    #[default]
    Clear,
    /// Revert the ticket to Backlog + comment (strict enforcement fired).
    Revert,
}

/// Graph state for one ticket's escalation decision. Serializable so the
/// `langgraph-core` checkpointer can persist the full decision record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationState {
    /// Ticket identifier, e.g. `FPCRM-123`.
    pub identifier: String,
    /// Was the ticket in a *started* state and missing dev-ready fields?
    /// (Computed from [`TicketReadiness::should_revert`] before the graph runs.)
    pub should_revert: bool,
    /// Human-readable list of the missing dimensions (for the audit trail).
    pub missing: Vec<String>,
    /// The adversarial judge's verdict: `Some(true)` = confirmed genuinely
    /// un-ready (revert), `Some(false)` = refuted (transient — clear), `None`
    /// = judge not queried (ticket was already clear).
    pub judge_confirmed: Option<bool>,
    /// The decision the graph reached.
    pub decision: Decision,
}

/// One checkpointed execution of the escalation graph.
#[derive(Debug, Clone, Serialize)]
pub struct EscalationGraphRun {
    /// Terminal graph state, including the graph-selected decision.
    pub state: EscalationState,
    /// Durable LangGraph checkpoint thread id for audit/replay.
    pub thread_id: String,
    /// Durable LangGraph checkpoint snapshots for audit/replay.
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<EscalationState>>,
    /// Per-channel checkpoint writes from LangGraph's write-history stream.
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    /// Typed LangGraph stream parts emitted by this decision run.
    pub stream: Vec<DecisionGraphStreamPart>,
    /// Compiled LangGraph topology that produced this run.
    pub topology: DecisionGraphTopology,
}

/// Proof that an enforcement graph checkpoint authorized a Linear revert.
#[derive(Debug, Clone)]
pub struct EnforcementRevertAuthorization {
    identifier: String,
    thread_id: String,
    checkpoint_id: String,
}

impl EnforcementRevertAuthorization {
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Durable enforcement graph thread that authorized the revert.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Durable decision-node checkpoint that authorized the revert.
    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    /// Stable audit reference for this concrete authorization checkpoint.
    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl EscalationGraphRun {
    /// Convert a terminal `Revert` graph result into a Linear revert authorization.
    #[must_use]
    pub fn revert_authorization(&self) -> Result<Option<EnforcementRevertAuthorization>, String> {
        if self.state.decision != Decision::Revert {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "enforcement",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(EnforcementRevertAuthorization {
            identifier: self.state.identifier.clone(),
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

impl EscalationState {
    /// Seed a state from a readiness snapshot (pre-graph, pre-judge).
    #[must_use]
    pub fn from_readiness(r: &TicketReadiness) -> Self {
        Self {
            identifier: r.identifier.clone(),
            should_revert: r.should_revert(),
            missing: r.missing_owned(),
            judge_confirmed: None,
            decision: Decision::Clear,
        }
    }
}

// Node ids (string-keyed in langgraph-core).
const CLASSIFY: &str = "classify";
const JUDGE: &str = "judge";
const REVERT: &str = "revert";
const CLEAR: &str = "clear";

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "enforcement")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn enforcement_state_schema() -> StateSchema<EscalationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "should_revert",
                "missing",
                "judge_confirmed",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "should_revert": { "type": "boolean" },
                "missing": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 }
                },
                "judge_confirmed": {
                    "anyOf": [
                        { "type": "null" },
                        { "type": "boolean" }
                    ]
                },
                "decision": { "type": "string", "enum": ["Clear", "Revert"] }
            },
            "x-sentinel": {
                "graph": "enforcement",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &EscalationState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "enforcement identifier must not be empty".to_string(),
                ));
            }
            if state.should_revert && state.missing.iter().all(|item| item.trim().is_empty()) {
                return Err(StateError::ValidationFailed(
                    "enforcement revert candidates must record missing readiness fields"
                        .to_string(),
                ));
            }
            if !state.should_revert && state.judge_confirmed.is_some() {
                return Err(StateError::ValidationFailed(
                    "enforcement judge verdict requires should_revert=true".to_string(),
                ));
            }
            if state.decision == Decision::Revert
                && !(state.should_revert && state.judge_confirmed == Some(true))
            {
                return Err(StateError::ValidationFailed(
                    "enforcement Revert requires should_revert=true and judge_confirmed=true"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

/// Build the strict-mode escalation graph with a durable env-selected checkpointer.
/// The nodes are deterministic state transitions; the async inputs
/// (`should_revert`, `judge_confirmed`) are computed by the orchestrator and
/// placed in the state before [`run_decision`] invokes the graph.
///
/// # Errors
/// Returns the stringified `langgraph-core` build/compile error on failure.
pub async fn build_escalation_graph(
) -> Result<langgraph_core::application::services::CompilationResult<EscalationState>, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("enforcement").await?;
    build_escalation_graph_with_checkpointer(checkpointer).await
}

/// Build the escalation graph with an ephemeral SQLite checkpointer.
#[cfg(test)]
async fn build_escalation_graph_with_ephemeral_sqlite(
) -> Result<langgraph_core::application::services::CompilationResult<EscalationState>, String> {
    build_escalation_graph_with_database_path(":memory:").await
}

/// Build the escalation graph against a caller-supplied SQLite checkpoint DB.
#[cfg(test)]
async fn build_escalation_graph_with_database_path(
    database_path: &str,
) -> Result<langgraph_core::application::services::CompilationResult<EscalationState>, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_escalation_graph_with_checkpointer(checkpointer).await
}

async fn build_escalation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<langgraph_core::application::services::CompilationResult<EscalationState>, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = enforcement_state_schema();
    let builder = StateGraphBuilder::<EscalationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        // classify: pass through; routing is decided by the conditional edge.
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: EscalationState| async move {
                emit_decision_node_event("enforcement", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        // judge: pass through; the verdict was set by the orchestrator.
        .add_async_node_with_config_and_error_handler(
            JUDGE,
            |s: EscalationState| async move {
                emit_decision_node_event("enforcement", JUDGE, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(
                JUDGE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        // revert / clear: record the terminal decision.
        .add_async_node_with_config_and_error_handler(
            REVERT,
            |s: EscalationState| async move {
                emit_decision_node_event("enforcement", REVERT, &s.identifier)?;
                let mut next = s;
                next.decision = Decision::Revert;
                Ok::<_, NodeError>(next)
            },
            node_config(
                REVERT,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            CLEAR,
            |s: EscalationState| async move {
                emit_decision_node_event("enforcement", CLEAR, &s.identifier)?;
                let mut next = s;
                next.decision = Decision::Clear;
                Ok::<_, NodeError>(next)
            },
            node_config(
                CLEAR,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        // classify → judge (started+unready) or clear (ready/not-started).
        .add_conditional_edge(CLASSIFY, |s: &EscalationState| {
            if s.should_revert {
                JUDGE.into()
            } else {
                CLEAR.into()
            }
        })
        // judge → revert (confirmed) or clear (refuted / unqueried).
        .add_conditional_edge(JUDGE, |s: &EscalationState| {
            if s.judge_confirmed == Some(true) {
                REVERT.into()
            } else {
                CLEAR.into()
            }
        })
        .add_edge(REVERT, END)
        .add_edge(CLEAR, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

/// The adversarial judge prompt: ask the model to CONFIRM (not assume) that a
/// started ticket is genuinely un-ready and should be reverted — defaulting to
/// "refute" when uncertain, so a transient signal never triggers a strict revert.
#[must_use]
pub fn judge_prompt(identifier: &str, missing: &[String]) -> String {
    format!(
        "A Linear ticket entered a STARTED state (In Progress / Code Review / …) while \
         appearing NOT dev-ready. Missing dimensions: [{missing}].\n\n\
         You are an adversarial verifier guarding a STRICT auto-revert. Confirm ONLY if \
         the ticket is genuinely un-ready and a human moving it forward would be a mistake. \
         REFUTE if this looks like a Linear API race, a label/estimate that is propagating, \
         or any transient state — when uncertain, REFUTE (false). A wrongful revert disrupts \
         real work, so the bar to confirm is high.\n\n\
         Ticket: {identifier}\n\
         Answer with a single word on the first line: CONFIRM or REFUTE, then a one-line reason.",
        missing = missing.join(", "),
    )
}

/// Interpret the judge's free-text reply into a confirm/refute boolean.
/// Conservative: only an explicit leading `CONFIRM` confirms; anything else
/// (including an unparseable reply) refutes, so the strict revert never fires
/// on an ambiguous judge response.
#[must_use]
pub fn parse_judge_verdict(reply: &str) -> bool {
    reply
        .trim_start()
        .lines()
        .next()
        .is_some_and(|first| first.trim().to_ascii_uppercase().starts_with("CONFIRM"))
}

/// Run the decision graph over a seeded state and return the graph run.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
pub async fn run_decision_report(
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
    state: EscalationState,
) -> Result<EscalationGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "enforcement",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "enforcement", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "enforcement",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(EscalationGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: escalation_graph_topology(compiled)?,
    })
}

/// Reflect the compiled escalation graph topology.
pub fn escalation_graph_topology(
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
) -> Result<DecisionGraphTopology, String> {
    topology("enforcement", compiled)
}

/// Run the decision graph over a seeded state and return the terminal decision.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
#[cfg(test)]
pub async fn run_decision(
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
    state: EscalationState,
) -> Result<Decision, String> {
    Ok(run_decision_report(compiled, state).await?.state.decision)
}

/// Full strict-mode evaluation for one ticket (the async orchestrator).
///
/// 1. If the ticket isn't a started-and-unready case → no judge call.
/// 2. Otherwise run the adversarial `Codex` judge (via `OpenRouter`), seed the
///    state with its verdict.
/// 3. Run the decision graph for every ticket so even `Clear` outcomes carry
///    a durable LangGraph checkpoint trail.
///
/// The caller performs the actual revert (`enforce_ticket`) when the returned
/// decision is `Revert` — keeping Linear mutation out of this decision path so
/// it stays testable and isolated from launcher swaps.
///
/// # Errors
/// Returns an error string if the judge call or graph execution fails.
pub async fn evaluate_ticket_report(
    llm: &dyn LlmPort,
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
    readiness: &TicketReadiness,
) -> Result<EscalationGraphRun, String> {
    let mut state = EscalationState::from_readiness(readiness);

    if state.should_revert {
        // Adversarial confirmation before a strict revert.
        let req = DelegationRequest {
            worker: Worker::Codex,
            task: judge_prompt(&state.identifier, &state.missing),
            context: String::new(),
            max_tokens: 256,
        };
        let verdict = match delegate(llm, &req).await {
            Ok(res) => {
                let v = parse_judge_verdict(&res.output);
                let reply = match res.output.lines().next() {
                    Some(line) => line,
                    None => "<empty>",
                };
                tracing::debug!(ticket = %state.identifier, confirmed = v, reply = %reply, "linear enforcer: judge verdict");
                v
            }
            Err(e) => {
                tracing::error!(ticket = %state.identifier, error = %e, "linear enforcer: judge call failed; graph decision aborted");
                return Err(format!(
                    "linear enforcer judge failed for {}: {e}",
                    state.identifier
                ));
            }
        };
        state.judge_confirmed = Some(verdict);
    }

    let run = run_decision_report(compiled, state).await?;
    tracing::debug!(
        ticket = %run.state.identifier,
        decision = ?run.state.decision,
        graph_thread_id = %run.thread_id,
        "linear enforcer: escalation decision"
    );
    Ok(run)
}

/// Full strict-mode evaluation for one ticket (the async orchestrator).
///
/// See [`evaluate_ticket_report`] for the graph-audit form.
///
/// # Errors
/// Returns an error string if the judge call or graph execution fails.
#[cfg(test)]
pub async fn evaluate_ticket(
    llm: &dyn LlmPort,
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
    readiness: &TicketReadiness,
) -> Result<EscalationState, String> {
    Ok(evaluate_ticket_report(llm, compiled, readiness)
        .await?
        .state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::port_errors::LlmError;
    use sentinel_domain::ports::{LlmPort, LlmRequest};

    struct PanicLlm;

    #[async_trait::async_trait]
    impl LlmPort for PanicLlm {
        async fn complete(
            &self,
            _request: LlmRequest,
        ) -> Result<String, sentinel_domain::port_errors::LlmError> {
            panic!("ready-ticket enforcement path must not call the judge")
        }
    }

    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmPort for FailingLlm {
        async fn complete(
            &self,
            _request: LlmRequest,
        ) -> Result<String, sentinel_domain::port_errors::LlmError> {
            Err(LlmError::Unavailable("offline".into()))
        }
    }

    fn readiness(state_type: &str, estimate: Option<f64>, ty: bool, area: bool) -> TicketReadiness {
        TicketReadiness {
            identifier: "FPCRM-1".into(),
            estimate,
            state_type: state_type.into(),
            state_name: "X".into(),
            has_type_label: ty,
            has_area_label: area,
            created_by_agent: true,
            priority: Some(3),
            sla_started_at: None,
            sla_breaches_at: None,
            has_acceptance_criteria: true,
            assignee_present: true,
            in_cycle: true,
            in_project: true,
            actively_blocked: false,
            sla_high_risk_at: None,
            due_date: None,
            has_milestone: true,
        }
    }

    #[test]
    fn parse_verdict_confirm_and_refute() {
        assert!(parse_judge_verdict("CONFIRM\ngenuinely missing estimate"));
        assert!(parse_judge_verdict("  confirm — no labels"));
        assert!(!parse_judge_verdict("REFUTE\nlabel is propagating"));
        assert!(!parse_judge_verdict("unsure"));
        assert!(!parse_judge_verdict(""), "empty reply must refute");
    }

    #[test]
    fn judge_prompt_lists_missing_and_demands_single_word() {
        let p = judge_prompt("FPCRM-9", &["estimate".into(), "Area label".into()]);
        assert!(p.contains("FPCRM-9"));
        assert!(p.contains("estimate, Area label"));
        assert!(p.contains("CONFIRM or REFUTE"));
        assert!(
            p.to_ascii_uppercase().contains("REFUTE"),
            "must bias to refute when uncertain"
        );
    }

    #[test]
    fn seed_from_ready_ticket_is_clear() {
        // Started + fully ready → should_revert false → Clear.
        let r = readiness("started", Some(3.0), true, true);
        let s = EscalationState::from_readiness(&r);
        assert!(!s.should_revert);
        assert_eq!(s.decision, Decision::Clear);
        assert_eq!(s.judge_confirmed, None);
    }

    #[test]
    fn seed_from_unready_started_ticket_flags_revert_candidate() {
        // Started + missing estimate → should_revert true (candidate for judge).
        let r = readiness("started", None, true, true);
        let s = EscalationState::from_readiness(&r);
        assert!(s.should_revert);
        assert!(s.missing.iter().any(|m| m == "estimate"));
    }

    #[tokio::test]
    async fn graph_routes_confirmed_unready_to_revert() {
        let compiled = build_escalation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = EscalationState {
            identifier: "FPCRM-2".into(),
            should_revert: true,
            missing: vec!["estimate".into()],
            judge_confirmed: Some(true),
            decision: Decision::Clear,
        };
        let d = run_decision(&compiled, state).await.expect("runs");
        assert_eq!(d, Decision::Revert, "started+unready+confirmed → Revert");
    }

    #[tokio::test]
    async fn revert_authorization_exists_only_for_revert_decision() {
        let compiled = build_escalation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let revert = run_decision_report(
            &compiled,
            EscalationState {
                identifier: "FPCRM-auth".into(),
                should_revert: true,
                missing: vec!["estimate".into()],
                judge_confirmed: Some(true),
                decision: Decision::Clear,
            },
        )
        .await
        .expect("runs");
        let auth = revert
            .revert_authorization()
            .expect("Revert run should authorize revert mutation")
            .expect("authorization");
        assert_eq!(auth.identifier(), "FPCRM-auth");
        assert_eq!(auth.thread_id(), revert.thread_id);
        let auth_checkpoint = revert
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == auth.checkpoint_id())
            .expect("authorization checkpoint must be present");
        assert_eq!(auth_checkpoint.source_node.as_deref(), Some(REVERT));
        assert_eq!(
            auth_checkpoint
                .writes
                .iter()
                .find(|write| write.channel == "state")
                .expect("authorization checkpoint state write")
                .node_id
                .as_str(),
            REVERT
        );
        assert_eq!(auth.checkpoint_ref(), auth_checkpoint.checkpoint_ref());

        let clear = run_decision_report(
            &compiled,
            EscalationState {
                identifier: "FPCRM-clear-auth".into(),
                should_revert: true,
                missing: vec!["estimate".into()],
                judge_confirmed: Some(false),
                decision: Decision::Clear,
            },
        )
        .await
        .expect("runs");
        assert!(
            clear
                .revert_authorization()
                .expect("authorization result")
                .is_none(),
            "Clear run must not authorize a Linear revert"
        );
    }

    #[tokio::test]
    async fn graph_routes_refuted_to_clear() {
        let compiled = build_escalation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = EscalationState {
            identifier: "FPCRM-3".into(),
            should_revert: true,
            missing: vec!["estimate".into()],
            judge_confirmed: Some(false), // judge refuted → no revert
            decision: Decision::Clear,
        };
        let d = run_decision(&compiled, state).await.expect("runs");
        assert_eq!(
            d,
            Decision::Clear,
            "judge refute → Clear (no wrongful revert)"
        );
    }

    #[tokio::test]
    async fn graph_routes_ready_ticket_to_clear() {
        let compiled = build_escalation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = EscalationState {
            identifier: "FPCRM-4".into(),
            should_revert: false, // dev-ready / not started
            missing: vec![],
            judge_confirmed: None,
            decision: Decision::Clear,
        };
        let d = run_decision(&compiled, state).await.expect("runs");
        assert_eq!(d, Decision::Clear, "ready ticket bypasses judge → Clear");
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_revert_authorization() {
        let compiled = build_escalation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let err = run_decision_report(
            &compiled,
            EscalationState {
                identifier: "FPCRM-forged".into(),
                should_revert: false,
                missing: vec![],
                judge_confirmed: None,
                decision: Decision::Revert,
            },
        )
        .await
        .expect_err("forged Revert state must fail LangGraph schema validation");
        assert!(
            err.contains("enforcement Revert requires"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn graph_persists_checkpoint_history_to_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("enforcement.db");
        let db = db.to_string_lossy().to_string();

        let compiled = build_escalation_graph_with_database_path(&db)
            .await
            .expect("graph builds");
        let state = EscalationState {
            identifier: "FPCRM-durable".into(),
            should_revert: true,
            missing: vec!["estimate".into()],
            judge_confirmed: Some(true),
            decision: Decision::Clear,
        };
        let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
            &compiled,
            "enforcement",
            &state.identifier,
            &state,
        )
        .expect("thread id");
        let run = run_decision_report(&compiled, state).await.expect("runs");
        assert_eq!(run.thread_id, thread_id);
        assert_eq!(run.state.decision, Decision::Revert);
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert_eq!(
            run.checkpoints.first().expect("latest").thread_id,
            thread_id
        );
        assert_eq!(
            run.checkpoints.first().expect("latest").state.decision,
            Decision::Revert
        );
        assert!(!run.stream.is_empty(), "run must expose stream parts");
        assert!(
            run.stream
                .iter()
                .any(|part| part.event_type == "ExecutionComplete"),
            "stream must expose LangGraph execution completion"
        );
        assert!(
            run.stream.iter().any(|part| part.payload_kind == "values"),
            "stream must expose LangGraph values payloads"
        );
        assert!(
            run.stream
                .iter()
                .any(|part| part.payload_kind == "checkpoints"),
            "stream must expose LangGraph checkpoint payloads"
        );
        assert!(
            run.stream.iter().any(|part| {
                part.payload_kind == "custom"
                    && part.payload_json["type"] == "sentinel.decision_node"
                    && part.payload_json["graph"] == "enforcement"
            }),
            "stream must expose Sentinel custom decision-node payloads"
        );
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state channel write history"
        );
        assert!(
            run.write_history
                .iter()
                .all(|write| write.value_len > 0 && write.value_sha256.len() == 64),
            "write history must expose value length and sha256"
        );
        assert!(
            run.write_history
                .iter()
                .filter(|write| write.channel == "state")
                .any(|write| write.value_json["decision"] == "Revert"),
            "state write history must decode the terminal decision JSON"
        );
        let state_writes = write_history(&compiled, &thread_id, Some("state"))
            .await
            .expect("state writes");
        assert!(state_writes.iter().all(|write| write.channel == "state"));
        assert_eq!(run.topology.graph, "enforcement");
        assert!(run.topology.durable_checkpointer);
        assert_eq!(run.topology.checkpointer_backend, "sqlite");
        assert!(
            run.topology
                .checkpointer_scope
                .starts_with("database_path:"),
            "topology must expose sanitized checkpoint scope"
        );
        assert_eq!(
            run.topology.schemas.state.as_ref().expect("state schema")["x-sentinel"]["graph"],
            "enforcement"
        );
        assert!(run.topology.schemas.input.is_some());
        assert!(run.topology.schemas.output.is_some());
        assert!(
            run.topology
                .nodes
                .iter()
                .all(|node| node.has_timeout_policy),
            "every enforcement graph node should carry a timeout policy"
        );
        assert!(
            run.topology.nodes.iter().any(|node| {
                node.id == JUDGE
                    && node.metadata.get("sentinel.graph").map(String::as_str)
                        == Some("enforcement")
                    && node
                        .metadata
                        .get("sentinel.checkpointer_backend")
                        .map(String::as_str)
                        == Some("sqlite")
                    && node
                        .metadata
                        .get("sentinel.checkpointer_scope")
                        .is_some_and(|scope| scope.starts_with("database_path:"))
            }),
            "topology must expose enforcement node metadata"
        );
        assert!(
            run.topology
                .edges
                .iter()
                .any(|edge| edge.kind == "conditional"),
            "topology must expose conditional routing"
        );

        let recompiled = build_escalation_graph_with_database_path(&db)
            .await
            .expect("graph rebuilds");
        let history = recompiled
            .get_state_history(&thread_id)
            .await
            .expect("history");
        assert!(!history.is_empty(), "checkpoint history must persist");
        assert_eq!(
            history.first().expect("latest").state().decision,
            Decision::Revert
        );
    }

    #[tokio::test]
    async fn graph_rerun_same_ticket_uses_fresh_thread_for_changed_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("enforcement-rerun.db");
        let db = db.to_string_lossy().to_string();
        let compiled = build_escalation_graph_with_database_path(&db)
            .await
            .expect("graph builds");

        let first = EscalationState {
            identifier: "FPCRM-rerun".into(),
            should_revert: true,
            missing: vec!["estimate".into()],
            judge_confirmed: Some(true),
            decision: Decision::Clear,
        };
        assert_eq!(
            run_decision(&compiled, first).await.expect("first run"),
            Decision::Revert
        );

        let second = EscalationState {
            identifier: "FPCRM-rerun".into(),
            should_revert: false,
            missing: vec![],
            judge_confirmed: None,
            decision: Decision::Clear,
        };
        assert_eq!(
            run_decision(&compiled, second).await.expect("second run"),
            Decision::Clear,
            "changed facts for the same ticket must not resume the stale revert checkpoint"
        );
    }

    #[tokio::test]
    async fn ready_ticket_evaluation_still_persists_clear_graph_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("enforcement-ready.db");
        let db = db.to_string_lossy().to_string();
        let compiled = build_escalation_graph_with_database_path(&db)
            .await
            .expect("graph builds");

        let ready = readiness("started", Some(3.0), true, true);
        let seed = EscalationState::from_readiness(&ready);
        let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
            &compiled,
            "enforcement",
            &seed.identifier,
            &seed,
        )
        .expect("thread id");

        let run = evaluate_ticket_report(&PanicLlm, &compiled, &ready)
            .await
            .expect("evaluates");
        assert_eq!(run.thread_id, thread_id);
        assert_eq!(run.state.decision, Decision::Clear);
        assert_eq!(
            run.state.judge_confirmed, None,
            "ready ticket must not invoke adversarial judge"
        );

        let recompiled = build_escalation_graph_with_database_path(&db)
            .await
            .expect("graph rebuilds");
        let history = recompiled
            .get_state_history(&thread_id)
            .await
            .expect("history");
        assert!(
            !history.is_empty(),
            "ready-ticket clear decision must still persist a LangGraph checkpoint"
        );
        assert_eq!(
            history.first().expect("latest").state().decision,
            Decision::Clear
        );
    }

    #[tokio::test]
    async fn judge_failure_aborts_without_clear_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("enforcement-judge-failure.db");
        let db = db.to_string_lossy().to_string();
        let compiled = build_escalation_graph_with_database_path(&db)
            .await
            .expect("graph builds");

        let unready = readiness("started", None, true, true);
        let mut stale_clear_state = EscalationState::from_readiness(&unready);
        stale_clear_state.judge_confirmed = Some(false);
        let stale_clear_thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
            &compiled,
            "enforcement",
            &stale_clear_state.identifier,
            &stale_clear_state,
        )
        .expect("thread id");

        let err = evaluate_ticket(&FailingLlm, &compiled, &unready)
            .await
            .expect_err("judge transport failure must abort");
        assert!(err.contains("judge failed"));

        let history = compiled
            .get_state_history(&stale_clear_thread_id)
            .await
            .expect("history lookup");
        assert!(
            history.is_empty(),
            "judge outage must not be converted into a durable Clear decision"
        );
    }
}
