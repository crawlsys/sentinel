//! Live integration test for the **`codex exec` subscription leg** with
//! `--output-schema` — opt-in (`--ignored`), requires the `codex` CLI on PATH
//! + an active Codex subscription + network. NOT part of the default suite.
//!
//! Why this exists: the subscription-first ladder's OpenAI leg shells out to
//! `codex exec --output-last-message --output-schema` and feeds the prompt on
//! STDIN. That contract depends on `codex`-CLI behavior we can only verify by
//! actually running it (the unit tests can't — they'd shell a real CLI). This
//! test catches a future codex-CLI change that would break the leg: a flag
//! rename, a stdin-handling change, or the OpenAI strict-schema rules
//! tightening (e.g. the `required`-must-list-every-key rule that already bit us
//! once).
//!
//! Run it:
//!   cargo test -p sentinel-infrastructure --test live_codex_schema -- --ignored
//!
//! Skips cleanly (passes) when `codex` is not installed, so an opt-in `--ignored`
//! run on a box without the CLI doesn't spuriously fail.

use sentinel_infrastructure::llm_scorer_runtime::{build_codex_cli_prompt_fn, resolve_cli};

/// Same shape as `rig_judge::JUDGE_VERDICT_SCHEMA` (kept in sync by eye — both
/// mirror `JudgeVerdict`). OpenAI strict structured-output requires EVERY
/// property in `required`; `requested_evidence` is nullable to stay "optional".
const JUDGE_VERDICT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["sufficient", "confidence", "reasoning", "requested_evidence"],
  "properties": {
    "sufficient": { "type": "boolean" },
    "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
    "reasoning": { "type": "string" },
    "requested_evidence": { "type": ["array", "null"], "items": { "type": "string" } }
  }
}"#;

#[tokio::test]
#[ignore = "live: requires `codex` CLI + Codex subscription + network; opt-in via --ignored"]
async fn codex_schema_leg_returns_valid_judge_verdict() {
    // Skip-as-pass when codex isn't installed — an --ignored run on a CI box
    // without the subscription CLI must not fail.
    if resolve_cli("codex").is_none() {
        eprintln!("codex not on PATH — skipping live schema integration test");
        return;
    }

    let (prompt_fn, provider) = build_codex_cli_prompt_fn("itest", Some(JUDGE_VERDICT_SCHEMA))
        .expect("codex present → builder must return Some");
    assert_eq!(provider, "codex-cli");

    let system =
        "You are an adversarial completion judge. Reply ONLY per the provided JSON schema."
            .to_string();
    let user = "Evidence: a test named test_login passed and exercises the login flow. \
                Is the work proven sufficient?"
        .to_string();

    // model_id is ignored by the CLI builder (codex uses its subscription model).
    let raw = prompt_fn("openai/gpt-5.5-pro".to_string(), system, user)
        .await
        .expect("codex schema call must succeed");

    // The whole point: the OpenAI leg returns guaranteed-valid JSON matching the
    // JudgeVerdict shape — parse it the same way the judge does.
    let v: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("codex output not valid JSON: {e}\nraw: {raw}"));
    assert!(v.get("sufficient").and_then(serde_json::Value::as_bool).is_some(), "missing/!bool sufficient: {raw}");
    let conf = v.get("confidence").and_then(serde_json::Value::as_f64).expect("confidence number");
    assert!((0.0..=1.0).contains(&conf), "confidence out of range: {conf}");
    assert!(v.get("reasoning").and_then(serde_json::Value::as_str).is_some(), "missing reasoning: {raw}");
    // requested_evidence must be present (schema requires it) — array or null.
    let re = v.get("requested_evidence").expect("requested_evidence key required by schema");
    assert!(re.is_array() || re.is_null(), "requested_evidence must be array|null: {re}");
}
