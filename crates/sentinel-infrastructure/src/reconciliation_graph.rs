//! Tier D truth-audit reconciliation graph — the dead-code catcher.
//!
//! A *Completed* Linear ticket is a CLAIM: "this shipped and is live." This
//! graph verifies that claim against fresh `origin/main`. The hard-won lesson
//! (FPROUTE-14): a feature can be *merged* yet dead — the function exists and
//! compiles, but nothing in the production path CALLS it. A naive ticket-ref
//! grep waves that through; only checking for a caller catches it.
//!
//! So the verdict is one of:
//! - `Shipped`      — code exists AND is reachable from a production caller.
//! - `DeadCode`     — code exists but has no production caller (FPROUTE-14).
//! - `Reverted`     — the code is gone from `origin/main` (closed, then undone).
//! - `Unverifiable` — infra/secret/external — can't be confirmed from code.
//!
//! Shape mirrors [`crate::enforcement_graph`]: a checkpointed `langgraph-core`
//! `StateGraph` (`classify → judge → flag | clear`) is the pure, replayable
//! DECISION; the I/O (Linear list, `git grep` over fresh main, the LLM judge)
//! runs in the async orchestrator; the graph nodes execute through
//! LangGraph-Rust's async runtime with durable thread IDs.
//!
//! Intended to run on a schedule (a sentinel cron), not on the live
//! subscription — it audits shipped history, it isn't a real-time gate.

use std::time::Duration;

use langgraph_core::application::services::GraphCompiler;
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
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

/// The reconciliation verdict for one Completed ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReconVerdict {
    /// Code exists and is reachable from a production caller — genuinely shipped.
    #[default]
    Shipped,
    /// Code exists but has no production caller — merged-but-dead (FPROUTE-14).
    DeadCode,
    /// The claimed code is gone from `origin/main` — reverted after closing.
    Reverted,
    /// Can't be confirmed from code (infra/secret/external dependency).
    Unverifiable,
}

impl ReconVerdict {
    /// Does this verdict mean the "Completed" claim is FALSE (needs flagging)?
    #[must_use]
    pub fn is_lie(self) -> bool {
        matches!(self, Self::DeadCode | Self::Reverted)
    }
}

/// What the graph decided to do about a Completed ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReconDecision {
    /// Leave the ticket alone — the claim holds (or can't be disproven).
    #[default]
    Clear,
    /// Flag the ticket: post an evidence comment + demote out of Completed.
    Flag,
}

/// Graph state for one Completed ticket's reconciliation. Serializable so the
/// checkpointer persists the full audit record (claim + code evidence + verdict).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconState {
    /// Ticket identifier, e.g. `FPROUTE-14`.
    pub identifier: String,
    /// The ticket's claim — title + a description/snippet the judge reasons over.
    pub claim: String,
    /// Code evidence: the result of searching fresh `origin/main` for the
    /// feature + its callers (built by the orchestrator before the graph runs).
    pub code_context: String,
    /// The judge's verdict (set by the orchestrator before the judge node).
    pub verdict: Option<ReconVerdict>,
    /// The decision the graph reached.
    pub decision: ReconDecision,
}

/// One checkpointed execution of the reconciliation graph.
#[derive(Debug, Clone, Serialize)]
pub struct ReconGraphRun {
    /// Terminal graph state, including the graph-selected decision.
    pub state: ReconState,
    /// Durable LangGraph checkpoint thread id for audit/replay.
    pub thread_id: String,
    /// Durable LangGraph checkpoint snapshots for audit/replay.
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<ReconState>>,
    /// Per-channel checkpoint writes from LangGraph's write-history stream.
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    /// Typed LangGraph stream parts emitted by this decision run.
    pub stream: Vec<DecisionGraphStreamPart>,
    /// Compiled LangGraph topology that produced this run.
    pub topology: DecisionGraphTopology,
}

/// Proof that a reconciliation graph checkpoint authorized flagging a ticket.
#[derive(Debug, Clone)]
pub struct ReconFlagAuthorization {
    identifier: String,
    thread_id: String,
    checkpoint_id: String,
    verdict: ReconVerdict,
}

impl ReconFlagAuthorization {
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Durable reconciliation graph thread that authorized the flag.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Durable reconciliation decision-node checkpoint that authorized the flag.
    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    /// Stable audit reference for this concrete authorization checkpoint.
    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }

    #[must_use]
    pub fn verdict(&self) -> ReconVerdict {
        self.verdict
    }
}

impl ReconGraphRun {
    /// Convert a terminal `Flag` graph result into a Linear flag authorization.
    #[must_use]
    pub fn flag_authorization(&self) -> Result<Option<ReconFlagAuthorization>, String> {
        if self.state.decision != ReconDecision::Flag {
            return Ok(None);
        }
        let verdict = self.state.verdict.ok_or_else(|| {
            format!(
                "reconciliation graph Flag decision for '{}' omitted verdict evidence",
                self.state.identifier
            )
        })?;
        let checkpoint_id = terminal_decision_checkpoint_result(
            "reconciliation",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(ReconFlagAuthorization {
            identifier: self.state.identifier.clone(),
            thread_id: self.thread_id.clone(),
            checkpoint_id,
            verdict,
        }))
    }
}

impl ReconState {
    /// Seed a state from a ticket claim + the code evidence (pre-judge).
    #[must_use]
    pub fn new(
        identifier: impl Into<String>,
        claim: impl Into<String>,
        code_context: impl Into<String>,
    ) -> Self {
        Self {
            identifier: identifier.into(),
            claim: claim.into(),
            code_context: code_context.into(),
            verdict: None,
            decision: ReconDecision::Clear,
        }
    }
}

// Node ids.
const CLASSIFY: &str = "classify";
const JUDGE: &str = "judge";
const FLAG: &str = "flag";
const CLEAR: &str = "clear";

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "reconciliation")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn reconciliation_state_schema() -> StateSchema<ReconState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["identifier", "claim", "code_context", "verdict", "decision"],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "claim": { "type": "string", "minLength": 1 },
                "code_context": { "type": "string" },
                "verdict": {
                    "anyOf": [
                        { "type": "null" },
                        {
                            "type": "string",
                            "enum": ["Shipped", "DeadCode", "Reverted", "Unverifiable"]
                        }
                    ]
                },
                "decision": { "type": "string", "enum": ["Clear", "Flag"] }
            },
            "x-sentinel": {
                "graph": "reconciliation",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &ReconState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "reconciliation identifier must not be empty".to_string(),
                ));
            }
            if state.claim.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "reconciliation claim must not be empty".to_string(),
                ));
            }
            if state.decision == ReconDecision::Flag
                && !state.verdict.is_some_and(ReconVerdict::is_lie)
            {
                return Err(StateError::ValidationFailed(
                    "reconciliation Flag requires a DeadCode or Reverted verdict".to_string(),
                ));
            }
            Ok(())
        })
}

/// Build the reconciliation decision graph with a durable env-selected checkpointer.
/// `classify → judge`; the judge verdict (set by the orchestrator) routes to
/// `flag` (the claim is a lie) or `clear` (shipped / unverifiable).
///
/// # Errors
/// Returns the stringified `langgraph-core` build/compile error on failure.
pub async fn build_reconciliation_graph(
) -> Result<langgraph_core::application::services::CompilationResult<ReconState>, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("reconciliation").await?;
    build_reconciliation_graph_with_checkpointer(checkpointer).await
}

/// Build the reconciliation graph with an ephemeral SQLite checkpointer.
#[cfg(test)]
async fn build_reconciliation_graph_with_ephemeral_sqlite(
) -> Result<langgraph_core::application::services::CompilationResult<ReconState>, String> {
    build_reconciliation_graph_with_database_path(":memory:").await
}

/// Build the reconciliation graph against a caller-supplied SQLite checkpoint DB.
#[cfg(test)]
async fn build_reconciliation_graph_with_database_path(
    database_path: &str,
) -> Result<langgraph_core::application::services::CompilationResult<ReconState>, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_reconciliation_graph_with_checkpointer(checkpointer).await
}

async fn build_reconciliation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<langgraph_core::application::services::CompilationResult<ReconState>, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = reconciliation_state_schema();
    let builder = StateGraphBuilder::<ReconState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: ReconState| async move {
                emit_decision_node_event("reconciliation", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            JUDGE,
            |s: ReconState| async move {
                emit_decision_node_event("reconciliation", JUDGE, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(
                JUDGE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            FLAG,
            |s: ReconState| async move {
                emit_decision_node_event("reconciliation", FLAG, &s.identifier)?;
                let mut next = s;
                next.decision = ReconDecision::Flag;
                Ok::<_, NodeError>(next)
            },
            node_config(
                FLAG,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            CLEAR,
            |s: ReconState| async move {
                emit_decision_node_event("reconciliation", CLEAR, &s.identifier)?;
                let mut next = s;
                next.decision = ReconDecision::Clear;
                Ok::<_, NodeError>(next)
            },
            node_config(
                CLEAR,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_edge(CLASSIFY, JUDGE)
        // judge → flag (verdict is a lie) or clear (shipped / unverifiable / unset).
        .add_conditional_edge(JUDGE, |s: &ReconState| {
            if s.verdict.is_some_and(ReconVerdict::is_lie) {
                FLAG.into()
            } else {
                CLEAR.into()
            }
        })
        .add_edge(FLAG, END)
        .add_edge(CLEAR, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

/// The reconciliation judge prompt. Gives the model the ticket claim + the
/// code-search evidence and asks for one of the four verdicts — explicitly
/// teaching the FPROUTE-14 "merged but no caller = dead" distinction, and
/// biasing to `UNVERIFIABLE` (not `SHIPPED`) when the evidence is inconclusive,
/// so a missing-caller never gets rubber-stamped as shipped.
#[must_use]
pub fn recon_judge_prompt(identifier: &str, claim: &str, code_context: &str) -> String {
    format!(
        "A Linear ticket is marked COMPLETED — a claim that its feature shipped and is LIVE. \
         Verify that claim against the code evidence from fresh `origin/main` below.\n\n\
         CRITICAL distinction (the FPROUTE-14 lesson): a function can be merged + compile + have \
         unit tests, yet be DEAD — nothing in the PRODUCTION path calls it. Existence is not \
         shipping; a production CALLER is. Check the evidence for a real caller, not just a definition.\n\n\
         Reply with exactly one verdict word on the first line, then a one-line reason:\n\
         - SHIPPED — the code exists AND is reached from a production caller.\n\
         - DEAD_CODE — the code exists but has no production caller (definition/tests only).\n\
         - REVERTED — the claimed code is absent from origin/main.\n\
         - UNVERIFIABLE — infra/secret/external; cannot be confirmed from code.\n\
         When the evidence is inconclusive, answer UNVERIFIABLE — never guess SHIPPED.\n\n\
         Ticket: {identifier}\n\
         Claim: {claim}\n\n\
         Code evidence (fresh origin/main):\n{code_context}",
    )
}

/// Parse the judge's reply into a [`ReconVerdict`]. Conservative: matches the
/// explicit verdict word on the first line; anything unrecognized →
/// `Unverifiable` (never a false `Shipped`, never a false `DeadCode`).
#[must_use]
pub fn parse_recon_verdict(reply: &str) -> ReconVerdict {
    let Some(first_line) = reply.trim_start().lines().next() else {
        return ReconVerdict::Unverifiable;
    };
    let first = first_line.trim().to_ascii_uppercase();
    if first.starts_with("SHIPPED") {
        ReconVerdict::Shipped
    } else if first.starts_with("DEAD_CODE")
        || first.starts_with("DEAD CODE")
        || first.starts_with("DEADCODE")
    {
        ReconVerdict::DeadCode
    } else if first.starts_with("REVERTED") {
        ReconVerdict::Reverted
    } else {
        ReconVerdict::Unverifiable
    }
}

/// Run the reconciliation graph over a seeded state, returning the graph run.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
pub async fn run_recon_decision_report(
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
    state: ReconState,
) -> Result<ReconGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "reconciliation",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "reconciliation", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "reconciliation",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(ReconGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: reconciliation_graph_topology(compiled)?,
    })
}

/// Reflect the compiled reconciliation graph topology.
pub fn reconciliation_graph_topology(
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
) -> Result<DecisionGraphTopology, String> {
    topology("reconciliation", compiled)
}

/// Run the reconciliation graph over a seeded state, returning the decision.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
#[cfg(test)]
pub async fn run_recon_decision(
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
    state: ReconState,
) -> Result<ReconDecision, String> {
    Ok(run_recon_decision_report(compiled, state)
        .await?
        .state
        .decision)
}

/// Reconcile one Completed ticket: run the adversarial `Codex` judge over the
/// claim + code evidence, then the decision graph. Returns the full state
/// (verdict + decision) so the caller can post the evidence comment + demote
/// when `decision == Flag`. Linear mutation stays in the caller — this path is
/// pure-decision and audit-safe.
///
/// # Errors
/// Returns an error string if the judge call or graph execution fails.
pub async fn reconcile_ticket_report(
    llm: &dyn LlmPort,
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
    mut state: ReconState,
) -> Result<ReconGraphRun, String> {
    let req = DelegationRequest {
        worker: Worker::Codex,
        task: recon_judge_prompt(&state.identifier, &state.claim, &state.code_context),
        context: String::new(),
        max_tokens: 512,
    };
    let verdict = match delegate(llm, &req).await {
        Ok(res) => parse_recon_verdict(&res.output),
        Err(e) => {
            tracing::error!(ticket = %state.identifier, error = %e, "linear reconciliation: judge call failed; graph decision aborted");
            return Err(format!(
                "linear reconciliation judge failed for {}: {e}",
                state.identifier
            ));
        }
    };
    state.verdict = Some(verdict);
    let run = run_recon_decision_report(compiled, state).await?;
    tracing::debug!(
        ticket = %run.state.identifier,
        verdict = ?run.state.verdict,
        decision = ?run.state.decision,
        graph_thread_id = %run.thread_id,
        "linear reconciliation: graph decision"
    );
    Ok(run)
}

/// Reconcile one Completed ticket and return only the terminal state.
///
/// See [`reconcile_ticket_report`] when the caller needs the checkpoint id or a
/// typed flag authorization for Linear mutation.
#[cfg(test)]
pub async fn reconcile_ticket(
    llm: &dyn LlmPort,
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
    state: ReconState,
) -> Result<ReconState, String> {
    let run = reconcile_ticket_report(llm, compiled, state).await?;
    Ok(run.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::port_errors::LlmError;
    use sentinel_domain::ports::{LlmPort, LlmRequest};

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

    #[test]
    fn verdict_lie_classification() {
        assert!(ReconVerdict::DeadCode.is_lie());
        assert!(ReconVerdict::Reverted.is_lie());
        assert!(!ReconVerdict::Shipped.is_lie());
        assert!(
            !ReconVerdict::Unverifiable.is_lie(),
            "unverifiable is not a flagged lie"
        );
    }

    #[test]
    fn parse_verdict_all_forms() {
        assert_eq!(
            parse_recon_verdict("SHIPPED\nhas caller in router"),
            ReconVerdict::Shipped
        );
        assert_eq!(
            parse_recon_verdict("DEAD_CODE\nno prod caller"),
            ReconVerdict::DeadCode
        );
        assert_eq!(
            parse_recon_verdict("dead code — tests only"),
            ReconVerdict::DeadCode
        );
        assert_eq!(
            parse_recon_verdict("REVERTED\ngone from main"),
            ReconVerdict::Reverted
        );
        assert_eq!(
            parse_recon_verdict("UNVERIFIABLE\nsecret"),
            ReconVerdict::Unverifiable
        );
        assert_eq!(
            parse_recon_verdict("¯\\_(ツ)_/¯"),
            ReconVerdict::Unverifiable,
            "garbage → Unverifiable"
        );
        assert_eq!(
            parse_recon_verdict(""),
            ReconVerdict::Unverifiable,
            "empty → Unverifiable"
        );
    }

    #[test]
    fn prompt_teaches_caller_distinction_and_biases_unverifiable() {
        let p = recon_judge_prompt(
            "FPROUTE-14",
            "Farthest Stop First optimizer",
            "fn farthest_first_tsp(...) // only in #[test]",
        );
        assert!(p.contains("FPROUTE-14"));
        assert!(p.contains("Farthest Stop First optimizer"));
        assert!(
            p.contains("PRODUCTION") || p.contains("production CALLER"),
            "must teach the caller check"
        );
        assert!(
            p.to_ascii_uppercase().contains("UNVERIFIABLE"),
            "must offer the conservative verdict"
        );
        assert!(p.contains("never guess SHIPPED"));
    }

    #[tokio::test]
    async fn graph_flags_dead_code() {
        let compiled = build_reconciliation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut s = ReconState::new("FPROUTE-14", "Farthest Stop First", "no caller");
        s.verdict = Some(ReconVerdict::DeadCode);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Flag, "dead-code Completed ticket → Flag");
    }

    #[tokio::test]
    async fn flag_authorization_exists_only_for_flag_decision() {
        let compiled = build_reconciliation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut flag = ReconState::new("FPROUTE-auth", "dead optimizer", "no caller");
        flag.verdict = Some(ReconVerdict::DeadCode);
        let flag_run = run_recon_decision_report(&compiled, flag)
            .await
            .expect("runs");
        let auth = flag_run
            .flag_authorization()
            .expect("Flag run should authorize Linear flag mutation")
            .expect("authorization");
        assert_eq!(auth.identifier(), "FPROUTE-auth");
        assert_eq!(auth.thread_id(), flag_run.thread_id);
        let auth_checkpoint = flag_run
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == auth.checkpoint_id())
            .expect("authorization checkpoint must be present");
        assert_eq!(auth_checkpoint.source_node.as_deref(), Some(FLAG));
        assert_eq!(
            auth_checkpoint
                .writes
                .iter()
                .find(|write| write.channel == "state")
                .expect("authorization checkpoint state write")
                .node_id
                .as_str(),
            FLAG
        );
        assert_eq!(auth.checkpoint_ref(), auth_checkpoint.checkpoint_ref());
        assert_eq!(auth.verdict(), ReconVerdict::DeadCode);

        let mut clear = ReconState::new("FPROUTE-clear-auth", "real feature", "prod caller");
        clear.verdict = Some(ReconVerdict::Shipped);
        let clear_run = run_recon_decision_report(&compiled, clear)
            .await
            .expect("runs");
        assert!(
            clear_run
                .flag_authorization()
                .expect("authorization result")
                .is_none(),
            "Clear run must not authorize Linear flag mutation"
        );
    }

    #[tokio::test]
    async fn graph_flags_reverted() {
        let compiled = build_reconciliation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut s = ReconState::new("FPCRM-12", "Nylas integration", "gone from main");
        s.verdict = Some(ReconVerdict::Reverted);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Flag, "reverted Completed ticket → Flag");
    }

    #[tokio::test]
    async fn graph_clears_shipped() {
        let compiled = build_reconciliation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut s = ReconState::new(
            "FPCRM-99",
            "Webhook delivery",
            "called in routes/webhooks.ts",
        );
        s.verdict = Some(ReconVerdict::Shipped);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Clear, "genuinely shipped → Clear");
    }

    #[tokio::test]
    async fn graph_clears_unverifiable() {
        let compiled = build_reconciliation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut s = ReconState::new("FPCRM-50", "Sentry wiring", "external secret");
        s.verdict = Some(ReconVerdict::Unverifiable);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(
            d,
            ReconDecision::Clear,
            "unverifiable → Clear (no wrongful flag)"
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_flag_authorization() {
        let compiled = build_reconciliation_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = ReconState::new("FPROUTE-forged", "claimed shipped", "prod caller");
        state.verdict = Some(ReconVerdict::Shipped);
        state.decision = ReconDecision::Flag;

        let err = run_recon_decision_report(&compiled, state)
            .await
            .expect_err("forged Flag state must fail LangGraph schema validation");
        assert!(
            err.contains("reconciliation Flag requires"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn graph_persists_checkpoint_history_to_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("reconciliation.db");
        let db = db.to_string_lossy().to_string();

        let compiled = build_reconciliation_graph_with_database_path(&db)
            .await
            .expect("graph builds");
        let mut state = ReconState::new("FPROUTE-durable", "dead optimizer", "no caller");
        state.verdict = Some(ReconVerdict::DeadCode);
        let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
            &compiled,
            "reconciliation",
            &state.identifier,
            &state,
        )
        .expect("thread id");
        let run = run_recon_decision_report(&compiled, state)
            .await
            .expect("runs");
        assert_eq!(run.thread_id, thread_id);
        assert_eq!(run.state.decision, ReconDecision::Flag);
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert_eq!(
            run.checkpoints.first().expect("latest").thread_id,
            thread_id
        );
        assert_eq!(
            run.checkpoints.first().expect("latest").state.decision,
            ReconDecision::Flag
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
                    && part.payload_json["graph"] == "reconciliation"
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
                .any(|write| write.value_json["decision"] == "Flag"),
            "state write history must decode the terminal decision JSON"
        );
        assert_eq!(run.topology.graph, "reconciliation");
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
            "reconciliation"
        );
        assert!(run.topology.schemas.input.is_some());
        assert!(run.topology.schemas.output.is_some());
        assert!(
            run.topology
                .nodes
                .iter()
                .all(|node| node.has_timeout_policy),
            "every reconciliation graph node should carry a timeout policy"
        );
        assert!(
            run.topology.nodes.iter().any(|node| {
                node.id == JUDGE
                    && node.metadata.get("sentinel.graph").map(String::as_str)
                        == Some("reconciliation")
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
            "topology must expose reconciliation node metadata"
        );
        assert!(
            run.topology
                .edges
                .iter()
                .any(|edge| edge.kind == "conditional"),
            "topology must expose conditional routing"
        );

        let recompiled = build_reconciliation_graph_with_database_path(&db)
            .await
            .expect("graph rebuilds");
        let history = recompiled
            .get_state_history(&thread_id)
            .await
            .expect("history");
        assert!(!history.is_empty(), "checkpoint history must persist");
        assert_eq!(
            history.first().expect("latest").state().decision,
            ReconDecision::Flag
        );
    }

    #[tokio::test]
    async fn graph_rerun_same_ticket_uses_fresh_thread_for_changed_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("reconciliation-rerun.db");
        let db = db.to_string_lossy().to_string();
        let compiled = build_reconciliation_graph_with_database_path(&db)
            .await
            .expect("graph builds");

        let mut first = ReconState::new("FPROUTE-rerun", "optimizer", "no caller");
        first.verdict = Some(ReconVerdict::DeadCode);
        assert_eq!(
            run_recon_decision(&compiled, first)
                .await
                .expect("first run"),
            ReconDecision::Flag
        );

        let mut second = ReconState::new("FPROUTE-rerun", "optimizer", "called in prod");
        second.verdict = Some(ReconVerdict::Shipped);
        assert_eq!(
            run_recon_decision(&compiled, second)
                .await
                .expect("second run"),
            ReconDecision::Clear,
            "changed evidence for the same ticket must not resume the stale flag checkpoint"
        );
    }

    #[tokio::test]
    async fn judge_failure_aborts_without_unverifiable_clear_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("reconciliation-judge-failure.db");
        let db = db.to_string_lossy().to_string();
        let compiled = build_reconciliation_graph_with_database_path(&db)
            .await
            .expect("graph builds");

        let state = ReconState::new("FPROUTE-fail", "claimed thing", "evidence");
        let mut stale_clear_state = state.clone();
        stale_clear_state.verdict = Some(ReconVerdict::Unverifiable);
        let stale_clear_thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
            &compiled,
            "reconciliation",
            &stale_clear_state.identifier,
            &stale_clear_state,
        )
        .expect("thread id");

        let err = reconcile_ticket(&FailingLlm, &compiled, state)
            .await
            .expect_err("judge transport failure must abort");
        assert!(err.contains("judge failed"));

        let history = compiled
            .get_state_history(&stale_clear_thread_id)
            .await
            .expect("history lookup");
        assert!(
            history.is_empty(),
            "judge outage must not be converted into a durable Unverifiable/Clear decision"
        );
    }
}
