//! End-to-end judge integration test — opt-in (`--ignored`), requires
//! `OPENROUTER_API_KEY` + network. NOT part of the default suite.
//!
//! Exercises the SAME code path `hook_cmd.rs` runs on `PostToolUse`: a
//! completed step-tool call -> `step_judge::process` -> the real
//! `MultiModelJudge` adapter -> OpenRouter -> a `Judged` verdict -> the
//! `judge_enforcement::Mode` gate that `submit_step_complete` consults.
//!
//! Run:
//!   OPENROUTER_API_KEY=... cargo test -p sentinel --test e2e_judge_integration \
//!     -- --ignored --nocapture

use std::collections::HashMap;

use sentinel_application::hooks::step_judge::{self, StepJudgeOutcome};
use sentinel_application::judge_enforcement::Mode;
use sentinel_domain::events::HookInput;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{PhaseSteps, SkillSteps, WorkflowStep};
use sentinel_infrastructure::rig_judge::MultiModelJudge;

fn demo_step_configs() -> HashMap<String, SkillSteps> {
    let step = WorkflowStep {
        id: "1".to_string(),
        description: "Prove the off-by-one in pagination::page_offset is fixed \
                      (returned one row too few on the last page)."
            .to_string(),
        blocker: true,
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
        extra: serde_json::Value::Null,
        r#override: None,
    };
    let phase = PhaseSteps {
        phase_id: "verify".to_string(),
        steps: vec![step],
    };
    let skill = SkillSteps {
        skill: "demo".to_string(),
        federation_version: "1.0.0".to_string(),
        phases: vec![phase],
    };
    let mut map = HashMap::new();
    map.insert("demo".to_string(), skill);
    map
}

fn sufficient_step_input() -> HookInput {
    HookInput {
        tool_name: Some("mcp__skills__demo__step_1".to_string()),
        session_id: Some("e2e-judge-session".to_string()),
        tool_input: Some(serde_json::json!({
            "summary": "Fixed page_offset inclusive bound; added tests.",
        })),
        tool_result: Some(serde_json::json!({
            "output": "running 3 tests\n\
                       test pagination::last_page_returns_all_rows ... ok\n\
                       test pagination::page_offset_inclusive_bound ... ok\n\
                       test pagination::empty_set_is_one_empty_page ... ok\n\
                       test result: ok. 3 passed; 0 failed\n\
                       diff: crates/api/src/pagination.rs (<= replaces <)"
        })),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "live network + OPENROUTER_API_KEY; opt-in via --ignored"]
async fn step_judge_produces_verdict_end_to_end() {
    let judge = MultiModelJudge::from_env();
    assert!(
        judge.has_any_provider(),
        "set OPENROUTER_API_KEY for the E2E judge test"
    );

    let input = sufficient_step_input();
    let mut state = SessionState::new("e2e-judge-session");
    let configs = demo_step_configs();

    let (output, outcome) =
        step_judge::process(&input, &mut state, &configs, &judge, false).await;

    assert!(
        output.blocked.is_none(),
        "step_judge must never block at PostToolUse; got {:?}",
        output.blocked
    );

    match outcome {
        StepJudgeOutcome::Judged {
            skill,
            phase_id,
            step_id,
            verdict,
            judge_model,
            ..
        } => {
            println!(
                "\n=== E2E JUDGE VERDICT ===\n\
                 skill={skill} phase={phase_id} step={step_id}\n\
                 model={}\nsufficient={} confidence={:.2}\nreasoning={}\n",
                judge_model.openrouter_model_id(),
                verdict.sufficient,
                verdict.confidence,
                verdict.reasoning,
            );
            assert_eq!(skill, "demo");
            assert_eq!(phase_id, "verify");
            assert_eq!(step_id, "1");
            assert!(
                verdict.sufficient,
                "recalibrated judge should PASS proven work; got insufficient: {}",
                verdict.reasoning
            );
        }
        other => panic!(
            "expected Judged outcome for a step tool with a live judge; got {other:?}"
        ),
    }
}

#[tokio::test]
#[ignore = "live network + OPENROUTER_API_KEY; opt-in via --ignored"]
async fn non_step_tool_is_noop_end_to_end() {
    let judge = MultiModelJudge::from_env();
    let input = HookInput {
        tool_name: Some("Bash".to_string()),
        session_id: Some("e2e-judge-session".to_string()),
        ..Default::default()
    };
    let mut state = SessionState::new("e2e-judge-session");
    let configs = demo_step_configs();

    let (output, outcome) =
        step_judge::process(&input, &mut state, &configs, &judge, false).await;
    assert!(output.blocked.is_none());
    assert!(
        matches!(outcome, StepJudgeOutcome::NotAStepTool),
        "non-step tools must be a no-op for step_judge; got {outcome:?}"
    );
}

#[test]
fn enforcement_mode_gate_logic() {
    assert!(!Mode::Shadow.blocks_seal());
    assert!(!Mode::Warn.blocks_seal());
    assert!(Mode::Enforce.blocks_seal());
    assert_eq!(Mode::from_env_with(|_| None), Mode::Shadow);
}
