//! Tier B enforcement escalation graph — the LangGraph-Rust decision spine.
//!
//! Strict mode: an un-ready *started* ticket is reverted immediately — but
//! never on a single raw signal. The decision runs through a checkpointed
//! `langgraph-core` `StateGraph` so every revert carries a durable, replayable
//! audit trail (which fields were missing, what the adversarial judge said,
//! why we acted). The graph is the DECISION; the I/O (Linear fetch/revert,
//! the LLM judge) happens in the async orchestrator around it, because
//! `langgraph-core` nodes are synchronous and cannot `.await`.
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

use std::sync::Arc;

use langgraph_core::application::services::GraphCompiler;
use langgraph_core::domain::value_objects::{NodeError, END, START};
use langgraph_core::{SqliteCheckpointer, StateGraphBuilder};
use serde::{Deserialize, Serialize};

use sentinel_application::delegation_service::{delegate, DelegationRequest, Worker};
use sentinel_domain::ports::LlmPort;

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
    /// = judge not consulted (ticket was already clear).
    pub judge_confirmed: Option<bool>,
    /// The decision the graph reached.
    pub decision: Decision,
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

/// Build the strict-mode escalation graph. Pure construction — no I/O. The
/// nodes are deterministic state transitions; the async inputs
/// (`should_revert`, `judge_confirmed`) are computed by the orchestrator and
/// placed in the state before [`run_decision`] invokes the graph.
///
/// # Errors
/// Returns the stringified `langgraph-core` build/compile error on failure.
pub async fn build_escalation_graph()
-> Result<langgraph_core::application::services::CompilationResult<EscalationState>, String> {
    let builder = StateGraphBuilder::<EscalationState>::new()
        // classify: pass through; routing is decided by the conditional edge.
        .add_node(CLASSIFY, |s: &EscalationState| Ok::<_, NodeError>(s.clone()))
        // judge: pass through; the verdict was set by the orchestrator.
        .add_node(JUDGE, |s: &EscalationState| Ok::<_, NodeError>(s.clone()))
        // revert / clear: record the terminal decision.
        .add_node(REVERT, |s: &EscalationState| {
            let mut next = s.clone();
            next.decision = Decision::Revert;
            Ok::<_, NodeError>(next)
        })
        .add_node(CLEAR, |s: &EscalationState| {
            let mut next = s.clone();
            next.decision = Decision::Clear;
            Ok::<_, NodeError>(next)
        })
        .add_edge(START, CLASSIFY)
        // classify → judge (started+unready) or clear (ready/not-started).
        .add_conditional_edge(CLASSIFY, |s: &EscalationState| {
            if s.should_revert { JUDGE.into() } else { CLEAR.into() }
        })
        // judge → revert (confirmed) or clear (refuted / unconsulted).
        .add_conditional_edge(JUDGE, |s: &EscalationState| {
            if s.judge_confirmed == Some(true) { REVERT.into() } else { CLEAR.into() }
        })
        .add_edge(REVERT, END)
        .add_edge(CLEAR, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    let checkpointer = SqliteCheckpointer::new(":memory:")
        .await
        .map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(Arc::new(checkpointer))
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

/// Run the decision graph over a seeded state and return the terminal decision.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
pub async fn run_decision(
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
    state: EscalationState,
) -> Result<Decision, String> {
    use langgraph_core::prelude::ExecutableGraph;
    let out = compiled.graph.invoke(state).await.map_err(|e| e.to_string())?;
    Ok(out.decision)
}

/// Full strict-mode evaluation for one ticket (the async orchestrator).
///
/// 1. If the ticket isn't a started-and-unready case → `Clear` immediately
///    (no judge call, no graph cost).
/// 2. Otherwise run the adversarial `Codex` judge (via `OpenRouter`), seed the
///    state with its verdict, and run the decision graph.
///
/// The caller performs the actual revert (`enforce_ticket`) when the returned
/// decision is `Revert` — keeping Linear mutation out of this decision path so
/// it stays testable and shadow-safe.
///
/// # Errors
/// Returns an error string if the judge call or graph execution fails.
pub async fn evaluate_ticket(
    llm: &dyn LlmPort,
    compiled: &langgraph_core::application::services::CompilationResult<EscalationState>,
    readiness: &TicketReadiness,
) -> Result<EscalationState, String> {
    let mut state = EscalationState::from_readiness(readiness);

    // Short-circuit: ready / not-started tickets never reach the judge.
    if !state.should_revert {
        state.decision = Decision::Clear;
        return Ok(state);
    }

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
            tracing::debug!(ticket = %state.identifier, confirmed = v, reply = %res.output.lines().next().unwrap_or(""), "linear enforcer: judge verdict");
            v
        }
        // Fail-safe: if the judge is unreachable, REFUTE (do not revert) — a
        // missed enforcement is far cheaper than a wrongful one.
        Err(e) => {
            tracing::warn!(ticket = %state.identifier, error = %e, "linear enforcer: judge call failed — refuting (no revert)");
            false
        }
    };
    state.judge_confirmed = Some(verdict);

    let decision = run_decision(compiled, state.clone()).await?;
    state.decision = decision;
    tracing::debug!(ticket = %state.identifier, ?decision, "linear enforcer: escalation decision");
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(p.to_ascii_uppercase().contains("REFUTE"), "must bias to refute when uncertain");
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
        let compiled = build_escalation_graph().await.expect("graph builds");
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
    async fn graph_routes_refuted_to_clear() {
        let compiled = build_escalation_graph().await.expect("graph builds");
        let state = EscalationState {
            identifier: "FPCRM-3".into(),
            should_revert: true,
            missing: vec!["estimate".into()],
            judge_confirmed: Some(false), // judge refuted → no revert
            decision: Decision::Clear,
        };
        let d = run_decision(&compiled, state).await.expect("runs");
        assert_eq!(d, Decision::Clear, "judge refute → Clear (no wrongful revert)");
    }

    #[tokio::test]
    async fn graph_routes_ready_ticket_to_clear() {
        let compiled = build_escalation_graph().await.expect("graph builds");
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
}
