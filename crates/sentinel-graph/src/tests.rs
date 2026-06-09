//! Unit tests for the phase-graph engine.
//!
//! The headline test is [`fresh_process_restores_checkpoint`]: it proves the
//! durable-execution property that the whole integration rests on — a graph
//! compiled in one "process" checkpoints to sqlite, and a *separately
//! compiled* graph (standing in for the next sentinel hook invocation) reads
//! that checkpoint back via `load_latest`.

use std::sync::Arc;

use sentinel_domain::judge::JudgeModel;
use sentinel_domain::workflow::{SkillWorkflow, WorkflowPhase};

use crate::{compile_skill_graph, next_phase_target, phase_saver, PhaseGraphState, Verdict};

fn phase(id: &str) -> WorkflowPhase {
    WorkflowPhase {
        id: id.to_string(),
        file: format!("{id}.md"),
        required: true,
        judge: JudgeModel::Sonnet,
        description: format!("{id} phase"),
        required_dyad: None,
    }
}

/// Minimal 3-phase workflow fixture.
fn fixture() -> SkillWorkflow {
    SkillWorkflow {
        skill: "linear".to_string(),
        phases: vec![phase("claim"), phase("fetch"), phase("review")],
        blocked_tool_prefixes: Vec::new(),
        blocked_bash_patterns: Vec::new(),
        bash_allowlist: Vec::new(),
    }
}

#[tokio::test]
async fn compiles_linear_workflow_with_all_phases() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");
    assert_eq!(
        graph.phase_ids(),
        &["claim".to_string(), "fetch".to_string(), "review".to_string()],
    );
}

#[tokio::test]
async fn empty_workflow_is_rejected() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let empty = SkillWorkflow {
        skill: "broken".to_string(),
        phases: vec![],
        blocked_tool_prefixes: Vec::new(),
        blocked_bash_patterns: Vec::new(),
        bash_allowlist: Vec::new(),
    };
    assert!(compile_skill_graph(&empty, saver).is_err());
}

#[tokio::test]
async fn graph_state_round_trips_through_workflow_state() {
    let mut s = PhaseGraphState::new(
        "linear",
        "sess-1",
        vec!["claim".into(), "fetch".into()],
    );
    s.current_phase = Some(1);
    s.completed_phases = vec!["claim".into()];
    s.last_verdict = Verdict::Pass;

    let ws = s.to_workflow_state();
    assert_eq!(ws.current_phase, Some(1));
    assert_eq!(ws.completed_phases, vec!["claim".to_string()]);

    let back = PhaseGraphState::from_workflow_state(&ws, vec!["claim".into(), "fetch".into()]);
    assert_eq!(back.current_phase, Some(1));
    assert_eq!(back.completed_phases, s.completed_phases);
    // Verdict is graph-transient — not carried on the domain type.
    assert_eq!(back.last_verdict, Verdict::Pending);
}

#[test]
fn pass_advances_to_next_phase() {
    let order = vec!["claim".to_string(), "fetch".to_string(), "review".to_string()];
    assert_eq!(next_phase_target(Verdict::Pass, 0, &order), "fetch");
    assert_eq!(next_phase_target(Verdict::Pass, 1, &order), "review");
}

#[test]
fn pass_on_last_phase_routes_to_end() {
    let order = vec!["claim".to_string(), "fetch".to_string(), "review".to_string()];
    assert_eq!(next_phase_target(Verdict::Pass, 2, &order), "__end__");
}

#[test]
fn fail_loops_back_to_same_phase() {
    let order = vec!["claim".to_string(), "fetch".to_string(), "review".to_string()];
    assert_eq!(next_phase_target(Verdict::Fail, 1, &order), "fetch");
}

#[test]
fn pending_stays_on_same_phase() {
    let order = vec!["claim".to_string(), "fetch".to_string()];
    assert_eq!(next_phase_target(Verdict::Pending, 0, &order), "claim");
}

/// THE durability proof: a checkpoint written by one compiled graph is read
/// back by a freshly-compiled graph sharing the same sqlite file — exactly
/// the cross-process-invocation contract sentinel's hook model needs.
#[tokio::test]
async fn fresh_process_restores_checkpoint() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let db = tmp.path().join("phases.db");
    let db_path = db.to_str().expect("utf8 path");

    let session = "sess-durable";
    let phase_order = vec!["claim".to_string(), "fetch".to_string(), "review".to_string()];

    // --- "process 1": compile, advance to phase 1, checkpoint ---
    {
        let saver = phase_saver(db_path).await.expect("saver-1");
        let graph = compile_skill_graph(&fixture(), Arc::clone(&saver)).expect("compile-1");

        let mut state = PhaseGraphState::new(session, session, phase_order.clone());
        state.current_phase = Some(1);
        state.completed_phases = vec!["claim".to_string()];

        graph
            .inner()
            .update_state(session, state)
            .await
            .expect("persist checkpoint");
    }

    // --- "process 2": fresh compile + saver on the SAME db, load_latest ---
    {
        let saver = phase_saver(db_path).await.expect("saver-2");
        let graph = compile_skill_graph(&fixture(), saver).expect("compile-2");

        let restored = graph
            .load_latest(session)
            .await
            .expect("load_latest ok")
            .expect("checkpoint present");

        assert_eq!(restored.current_phase, Some(1));
        assert_eq!(restored.completed_phases, vec!["claim".to_string()]);
    }
}

#[tokio::test]
async fn apply_verdict_pass_advances_and_persists() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");

    let s = graph
        .apply_verdict("linear", "sess-pass", "claim", true)
        .await
        .expect("apply pass");
    assert_eq!(s.completed_phases, vec!["claim".to_string()]);
    assert_eq!(s.current_phase, Some(1)); // advanced to fetch
    assert!(!s.complete);

    // Re-load proves it persisted as a checkpoint.
    let reloaded = graph.load_latest("sess-pass").await.expect("load").expect("present");
    assert_eq!(reloaded.completed_phases, vec!["claim".to_string()]);
    assert_eq!(reloaded.current_phase, Some(1));
}

#[tokio::test]
async fn apply_verdict_fail_keeps_phase() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");

    let s = graph
        .apply_verdict("linear", "sess-fail", "claim", false)
        .await
        .expect("apply fail");
    assert!(s.completed_phases.is_empty());
    assert_eq!(s.current_phase, Some(0)); // stayed on claim
    assert!(!s.complete);
}

#[tokio::test]
async fn apply_verdict_pass_on_last_phase_completes() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");

    graph.apply_verdict("linear", "s", "claim", true).await.expect("p1");
    graph.apply_verdict("linear", "s", "fetch", true).await.expect("p2");
    let s = graph.apply_verdict("linear", "s", "review", true).await.expect("p3");

    assert_eq!(
        s.completed_phases,
        vec!["claim".to_string(), "fetch".to_string(), "review".to_string()],
    );
    assert!(s.complete);
}

#[tokio::test]
async fn apply_verdict_unknown_phase_errors() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");
    assert!(graph
        .apply_verdict("linear", "s", "nonexistent", true)
        .await
        .is_err());
}

#[tokio::test]
async fn phase_history_accumulates_checkpoints() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");

    graph.apply_verdict("linear", "h", "claim", true).await.expect("p1");
    graph.apply_verdict("linear", "h", "fetch", true).await.expect("p2");

    let history = graph.phase_history("h").await.expect("history");
    // At least one checkpoint per verdict; latest reflects 2 completed phases.
    assert!(history.len() >= 2, "expected >=2 checkpoints, got {}", history.len());
    let latest = history.last().expect("non-empty");
    assert_eq!(latest.completed_phases, vec!["claim".to_string(), "fetch".to_string()]);
}

/// Time-travel: complete claim+fetch, then replay fetch — it forks back to a
/// state where fetch is no longer completed and `current_phase` points at fetch.
#[tokio::test]
async fn replay_phase_forks_before_target() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");

    graph.apply_verdict("linear", "r", "claim", true).await.expect("p1");
    graph.apply_verdict("linear", "r", "fetch", true).await.expect("p2");

    let forked = graph.replay_phase("linear", "r", "fetch").await.expect("replay");
    // claim stays done (it's before fetch); fetch is dropped for the re-run.
    assert_eq!(forked.completed_phases, vec!["claim".to_string()]);
    assert_eq!(forked.current_phase, Some(1)); // back on fetch
    assert!(!forked.complete);

    // The fork persisted as the new latest checkpoint.
    let latest = graph.load_latest("r").await.expect("load").expect("present");
    assert_eq!(latest.completed_phases, vec!["claim".to_string()]);
}

#[tokio::test]
async fn replay_unknown_phase_errors() {
    let saver = phase_saver(":memory:").await.expect("saver");
    let graph = compile_skill_graph(&fixture(), saver).expect("compile");
    assert!(graph.replay_phase("linear", "r", "ghost").await.is_err());
}
