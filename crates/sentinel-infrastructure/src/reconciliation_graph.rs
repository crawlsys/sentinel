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
//! runs in the async orchestrator around it, because nodes are synchronous.
//!
//! Intended to run on a schedule (a sentinel cron), not on the live
//! subscription — it audits shipped history, it isn't a real-time gate.

use std::sync::Arc;

use langgraph_core::application::services::GraphCompiler;
use langgraph_core::domain::value_objects::{NodeError, END, START};
use langgraph_core::{SqliteCheckpointer, StateGraphBuilder};
use serde::{Deserialize, Serialize};

use sentinel_application::delegation_service::{delegate, DelegationRequest, Worker};
use sentinel_domain::ports::LlmPort;

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

impl ReconState {
    /// Seed a state from a ticket claim + the code evidence (pre-judge).
    #[must_use]
    pub fn new(identifier: impl Into<String>, claim: impl Into<String>, code_context: impl Into<String>) -> Self {
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

/// Build the reconciliation decision graph. Pure construction, no I/O.
/// `classify → judge`; the judge verdict (set by the orchestrator) routes to
/// `flag` (the claim is a lie) or `clear` (shipped / unverifiable).
///
/// # Errors
/// Returns the stringified `langgraph-core` build/compile error on failure.
pub async fn build_reconciliation_graph()
-> Result<langgraph_core::application::services::CompilationResult<ReconState>, String> {
    let builder = StateGraphBuilder::<ReconState>::new()
        .add_node(CLASSIFY, |s: &ReconState| Ok::<_, NodeError>(s.clone()))
        .add_node(JUDGE, |s: &ReconState| Ok::<_, NodeError>(s.clone()))
        .add_node(FLAG, |s: &ReconState| {
            let mut next = s.clone();
            next.decision = ReconDecision::Flag;
            Ok::<_, NodeError>(next)
        })
        .add_node(CLEAR, |s: &ReconState| {
            let mut next = s.clone();
            next.decision = ReconDecision::Clear;
            Ok::<_, NodeError>(next)
        })
        .add_edge(START, CLASSIFY)
        .add_edge(CLASSIFY, JUDGE)
        // judge → flag (verdict is a lie) or clear (shipped / unverifiable / unset).
        .add_conditional_edge(JUDGE, |s: &ReconState| {
            if s.verdict.is_some_and(ReconVerdict::is_lie) { FLAG.into() } else { CLEAR.into() }
        })
        .add_edge(FLAG, END)
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
    let first = reply.trim_start().lines().next().unwrap_or("").trim().to_ascii_uppercase();
    if first.starts_with("SHIPPED") {
        ReconVerdict::Shipped
    } else if first.starts_with("DEAD_CODE") || first.starts_with("DEAD CODE") || first.starts_with("DEADCODE") {
        ReconVerdict::DeadCode
    } else if first.starts_with("REVERTED") {
        ReconVerdict::Reverted
    } else {
        ReconVerdict::Unverifiable
    }
}

/// Run the reconciliation graph over a seeded state, returning the decision.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
pub async fn run_recon_decision(
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
    state: ReconState,
) -> Result<ReconDecision, String> {
    use langgraph_core::prelude::ExecutableGraph;
    let out = compiled.graph.invoke(state).await.map_err(|e| e.to_string())?;
    Ok(out.decision)
}

/// Reconcile one Completed ticket: run the adversarial `Codex` judge over the
/// claim + code evidence, then the decision graph. Returns the full state
/// (verdict + decision) so the caller can post the evidence comment + demote
/// when `decision == Flag`. Linear mutation stays in the caller — this path is
/// pure-decision and audit-safe.
///
/// # Errors
/// Returns an error string if the judge call or graph execution fails.
pub async fn reconcile_ticket(
    llm: &dyn LlmPort,
    compiled: &langgraph_core::application::services::CompilationResult<ReconState>,
    mut state: ReconState,
) -> Result<ReconState, String> {
    let req = DelegationRequest {
        worker: Worker::Codex,
        task: recon_judge_prompt(&state.identifier, &state.claim, &state.code_context),
        context: String::new(),
        max_tokens: 512,
    };
    let verdict = match delegate(llm, &req).await {
        Ok(res) => parse_recon_verdict(&res.output),
        // Fail-safe: judge unreachable ⇒ Unverifiable (never a false flag).
        Err(_) => ReconVerdict::Unverifiable,
    };
    state.verdict = Some(verdict);
    state.decision = run_recon_decision(compiled, state.clone()).await?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_lie_classification() {
        assert!(ReconVerdict::DeadCode.is_lie());
        assert!(ReconVerdict::Reverted.is_lie());
        assert!(!ReconVerdict::Shipped.is_lie());
        assert!(!ReconVerdict::Unverifiable.is_lie(), "unverifiable is not a flagged lie");
    }

    #[test]
    fn parse_verdict_all_forms() {
        assert_eq!(parse_recon_verdict("SHIPPED\nhas caller in router"), ReconVerdict::Shipped);
        assert_eq!(parse_recon_verdict("DEAD_CODE\nno prod caller"), ReconVerdict::DeadCode);
        assert_eq!(parse_recon_verdict("dead code — tests only"), ReconVerdict::DeadCode);
        assert_eq!(parse_recon_verdict("REVERTED\ngone from main"), ReconVerdict::Reverted);
        assert_eq!(parse_recon_verdict("UNVERIFIABLE\nsecret"), ReconVerdict::Unverifiable);
        assert_eq!(parse_recon_verdict("¯\\_(ツ)_/¯"), ReconVerdict::Unverifiable, "garbage → Unverifiable");
        assert_eq!(parse_recon_verdict(""), ReconVerdict::Unverifiable, "empty → Unverifiable");
    }

    #[test]
    fn prompt_teaches_caller_distinction_and_biases_unverifiable() {
        let p = recon_judge_prompt("FPROUTE-14", "Farthest Stop First optimizer", "fn farthest_first_tsp(...) // only in #[test]");
        assert!(p.contains("FPROUTE-14"));
        assert!(p.contains("Farthest Stop First optimizer"));
        assert!(p.contains("PRODUCTION") || p.contains("production CALLER"), "must teach the caller check");
        assert!(p.to_ascii_uppercase().contains("UNVERIFIABLE"), "must offer the conservative verdict");
        assert!(p.contains("never guess SHIPPED"));
    }

    #[tokio::test]
    async fn graph_flags_dead_code() {
        let compiled = build_reconciliation_graph().await.expect("graph builds");
        let mut s = ReconState::new("FPROUTE-14", "Farthest Stop First", "no caller");
        s.verdict = Some(ReconVerdict::DeadCode);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Flag, "dead-code Completed ticket → Flag");
    }

    #[tokio::test]
    async fn graph_flags_reverted() {
        let compiled = build_reconciliation_graph().await.expect("graph builds");
        let mut s = ReconState::new("FPCRM-12", "Nylas integration", "gone from main");
        s.verdict = Some(ReconVerdict::Reverted);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Flag, "reverted Completed ticket → Flag");
    }

    #[tokio::test]
    async fn graph_clears_shipped() {
        let compiled = build_reconciliation_graph().await.expect("graph builds");
        let mut s = ReconState::new("FPCRM-99", "Webhook delivery", "called in routes/webhooks.ts");
        s.verdict = Some(ReconVerdict::Shipped);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Clear, "genuinely shipped → Clear");
    }

    #[tokio::test]
    async fn graph_clears_unverifiable() {
        let compiled = build_reconciliation_graph().await.expect("graph builds");
        let mut s = ReconState::new("FPCRM-50", "Sentry wiring", "external secret");
        s.verdict = Some(ReconVerdict::Unverifiable);
        let d = run_recon_decision(&compiled, s).await.expect("runs");
        assert_eq!(d, ReconDecision::Clear, "unverifiable → Clear (no wrongful flag)");
    }
}
